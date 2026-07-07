#!/usr/bin/env python3
"""Vérificateur de mise à jour NovaDesk — squelette du client (plan 15 §15.6.4).

Reproduit ce que le **client** ferait avant d'appliquer une mise à jour :
  1. charger la racine de confiance `root.json` (clés + rôles + seuils) ;
  2. vérifier les **signatures Ed25519** du manifeste de canal contre le rôle
     `targets` (seuil de signatures atteint) ;
  3. contrôler l'expiration (`expires`) ;
  4. rendre une **décision** identique à `UpdateService::check_update`
     (`server/nd-api/src/update.rs`) : UpToDate / UpdateAvailable / ForcedUpdate,
     par comparaison sémantique de versions.

C'est la moitié « signature applicative » de l'invariant de double vérification :
la signature native de l'OS (Authenticode/codesign/GPG) reste faite en amont par
l'installeur ; ici on valide notre propre chaîne, indépendante des CA.

Usage :
  verify_update.py verify-root --root root.json
  verify_update.py verify --root root.json --manifest manifest.stable.json \
                   [--current 0.1.0] [--platform windows-x86_64]

Code de sortie 0 = signatures valides (et, si --current fourni, la décision est
imprimée). Non-zéro = signature invalide, expiré, ou racine incohérente.
"""
from __future__ import annotations

import argparse
import binascii
import datetime as _dt
import hashlib
import json
import sys
from pathlib import Path

try:
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
except ImportError:  # pragma: no cover
    sys.stderr.write("erreur : module 'cryptography' requis (pip install cryptography).\n")
    raise SystemExit(2)


def canonical(obj) -> bytes:
    """Doit être identique bit à bit à celle du signeur (sign_manifest.py)."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode(
        "utf-8"
    )


def keyid_of(public_hex: str) -> str:
    return hashlib.sha256(public_hex.encode("ascii")).hexdigest()


def parse_version(v: str) -> tuple[int, int, int]:
    parts = v.strip().split(".")
    if len(parts) != 3:
        raise ValueError(f"version « {v} » : format major.minor.patch attendu")
    return tuple(int(p) for p in parts)  # type: ignore[return-value]


def _now() -> _dt.datetime:
    return _dt.datetime.now(_dt.timezone.utc)


def _expire(signed: dict) -> None:
    exp = signed.get("expires")
    if not exp:
        return
    quand = _dt.datetime.strptime(exp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=_dt.timezone.utc)
    if quand < _now():
        raise SystemExit(f"métadonnées expirées le {exp}")


def verifier_signatures(doc: dict, cles: dict, role: dict) -> int:
    """Vérifie les signatures de `doc` contre `role` ; renvoie le nb de bonnes.

    `cles` : keyid -> clé publique hex. `role` : {keyids, threshold}.
    Lève SystemExit si le seuil n'est pas atteint.
    """
    signed_bytes = canonical(doc["signed"])
    autorises = set(role["keyids"])
    bonnes = 0
    vues: set[str] = set()
    for sig in doc.get("signatures", []):
        kid = sig.get("keyid", "")
        if kid not in autorises or kid in vues or sig.get("algo") != "ed25519":
            continue
        pub_hex = cles.get(kid)
        if not pub_hex:
            continue
        try:
            Ed25519PublicKey.from_public_bytes(binascii.unhexlify(pub_hex)).verify(
                binascii.unhexlify(sig["sig"]), signed_bytes
            )
        except (InvalidSignature, binascii.Error, ValueError):
            continue
        vues.add(kid)
        bonnes += 1
    seuil = int(role["threshold"])
    if bonnes < seuil:
        raise SystemExit(f"seuil de signatures non atteint : {bonnes}/{seuil}")
    return bonnes


def charger_racine(chemin: Path) -> dict:
    root = json.loads(chemin.read_text(encoding="utf-8"))
    signed = root["signed"]
    _expire(signed)
    cles = {kid: k["keyval"]["public"] for kid, k in signed["keys"].items()}
    # La racine s'auto-authentifie : ses signatures satisfont son propre rôle root.
    verifier_signatures(root, cles, signed["roles"]["root"])
    return {"cles": cles, "roles": signed["roles"]}


def decision(manifest_signed: dict, current: str) -> str:
    """Réplique la logique de `UpdateService::check_update` (update.rs)."""
    cur = parse_version(current)
    latest = parse_version(manifest_signed["latest"])
    mini = parse_version(manifest_signed["min_supported"])
    if cur < mini:
        return f"ForcedUpdate (client {current} < min_supported {manifest_signed['min_supported']})"
    if cur < latest:
        return f"UpdateAvailable (client {current} < latest {manifest_signed['latest']})"
    return f"UpToDate (client {current} >= latest {manifest_signed['latest']})"


def cmd_verify_root(args) -> int:
    charger_racine(Path(args.root))
    print(f"root.json : signatures valides, {Path(args.root)} de confiance.")
    return 0


def cmd_verify(args) -> int:
    racine = charger_racine(Path(args.root))
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    signed = manifest["signed"]
    _expire(signed)
    n = verifier_signatures(manifest, racine["cles"], racine["roles"]["targets"])
    print(f"manifeste « {signed.get('channel', '?')} » : {n} signature(s) valide(s).")
    if args.platform:
        arts = [a for a in signed.get("artifacts", []) if a.get("platform") == args.platform]
        if not arts:
            print(f"  (aucun artefact pour la plateforme {args.platform})")
        for a in arts:
            print(f"  artefact {a['platform']}/{a['kind']} sha256={a['sha256'][:16]}… {a['url']}")
    if args.current:
        print(f"décision : {decision(signed, args.current)}")
    return 0


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description="Vérificateur de mise à jour NovaDesk.")
    sub = p.add_subparsers(dest="cmd", required=True)

    vr = sub.add_parser("verify-root", help="auto-vérifie root.json.")
    vr.add_argument("--root", required=True)
    vr.set_defaults(func=cmd_verify_root)

    v = sub.add_parser("verify", help="vérifie un manifeste de canal.")
    v.add_argument("--root", required=True)
    v.add_argument("--manifest", required=True)
    v.add_argument("--current", help="version courante du client (x.y.z).")
    v.add_argument("--platform", help="filtre l'artefact (ex. windows-x86_64).")
    v.set_defaults(func=cmd_verify)

    args = p.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
