#!/usr/bin/env python3
"""Signature des métadonnées de mise à jour NovaDesk (squelette TUF-like).

Aligne le format sur le service `UpdateService` de `server/nd-api/src/update.rs`
(canaux stable/beta/canary/lts, `latest`, `min_supported`, `sha256`, delta) et
ajoute la couche que le cœur Rust ne porte pas encore : des **métadonnées
signées Ed25519** (indépendantes des CA de l'OS), vérifiées par le client avant
d'appliquer une mise à jour (plan 15 §15.6.4 — « invariant de double
vérification »).

Modèle de confiance (simplifié, mono-clé de démonstration) :
  root.json      → liste les clés de confiance et les rôles (root/targets/…) ;
  manifest.*.json → métadonnées d'un canal, signées par la clé « targets ».

En production, chaque rôle porte une clé DISTINCTE et la clé « root » vit
**hors ligne** (HSM) ; ici une seule clé de démonstration sert tous les rôles
pour rester vérifiable de bout en bout sans infrastructure.

Sous-commandes :
  keygen  --out FICHIER [--demo]        génère une graine Ed25519 (hex, 32 o).
  pubkey  --key FICHIER                 affiche clé publique + keyid.
  emit-root --key FICHIER --out ROOT    fabrique et signe root.json.
  sign    --key FICHIER --in M --out S  signe le champ `signed` d'un manifeste.

La signature porte sur le **JSON canonique** du sous-objet `signed`
(clés triées, séparateurs compacts, UTF-8) — même règle que le vérificateur.

⚠ Aucune clé de production n'est versionnée. La graine de démonstration est
déterministe (documentée) pour que les manifestes d'exemple soient reproductibles.
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
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
except ImportError:  # pragma: no cover - dépend de l'environnement de build
    sys.stderr.write(
        "erreur : le module 'cryptography' est requis "
        "(pip install cryptography) pour signer les manifestes.\n"
    )
    raise SystemExit(2)

# Graine de démonstration DÉTERMINISTE — jamais pour la production.
# Documentée pour que quiconque régénère les mêmes clé/manifestes d'exemple.
DEMO_SEED = hashlib.sha256(b"novadesk-demo-tuf-root-key").digest()


def canonical(obj) -> bytes:
    """JSON canonique : clés triées, compact, UTF-8 (règle commune signer/vérif)."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode(
        "utf-8"
    )


def keyid_of(public_hex: str) -> str:
    """Identifiant de clé = SHA-256 de la clé publique hexadécimale."""
    return hashlib.sha256(public_hex.encode("ascii")).hexdigest()


def load_seed(path: Path) -> bytes:
    seed = binascii.unhexlify(path.read_text(encoding="ascii").strip())
    if len(seed) != 32:
        raise SystemExit(f"graine invalide : 32 octets attendus, {len(seed)} lus")
    return seed


def signer_de(seed: bytes) -> Ed25519PrivateKey:
    return Ed25519PrivateKey.from_private_bytes(seed)


def public_hex_de(seed: bytes) -> str:
    from cryptography.hazmat.primitives import serialization

    pub = signer_de(seed).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )
    return pub.hex()


def signer_objet(seed: bytes, signed: dict) -> dict:
    """Enveloppe `{signed, signatures:[…]}` pour un sous-objet `signed`."""
    pub_hex = public_hex_de(seed)
    sig = signer_de(seed).sign(canonical(signed)).hex()
    return {
        "signed": signed,
        "signatures": [{"keyid": keyid_of(pub_hex), "algo": "ed25519", "sig": sig}],
    }


def cmd_keygen(args) -> int:
    seed = DEMO_SEED if args.demo else __import__("os").urandom(32)
    Path(args.out).write_text(seed.hex() + "\n", encoding="ascii")
    pub = public_hex_de(seed)
    tag = " (DÉMO déterministe)" if args.demo else ""
    print(f"graine écrite dans {args.out}{tag}")
    print(f"cle_publique : {pub}")
    print(f"keyid        : {keyid_of(pub)}")
    return 0


def cmd_pubkey(args) -> int:
    pub = public_hex_de(load_seed(Path(args.key)))
    print(f"cle_publique : {pub}")
    print(f"keyid        : {keyid_of(pub)}")
    return 0


def _expiry(jours: int) -> str:
    quand = _dt.datetime(2026, 7, 4, tzinfo=_dt.timezone.utc) + _dt.timedelta(days=jours)
    return quand.strftime("%Y-%m-%dT%H:%M:%SZ")


def cmd_emit_root(args) -> int:
    seed = load_seed(Path(args.key))
    pub_hex = public_hex_de(seed)
    kid = keyid_of(pub_hex)
    role = {"keyids": [kid], "threshold": 1}
    signed = {
        "_type": "root",
        "spec_version": "1.0",
        "version": 1,
        "expires": _expiry(365),
        "keys": {kid: {"keytype": "ed25519", "scheme": "ed25519", "keyval": {"public": pub_hex}}},
        # Démonstration : une seule clé pour tous les rôles. PRODUCTION : une clé
        # hors ligne par rôle (root séparé, rotation indépendante).
        "roles": {r: dict(role) for r in ("root", "timestamp", "snapshot", "targets")},
    }
    Path(args.out).write_text(json.dumps(signer_objet(seed, signed), indent=2) + "\n", "utf-8")
    print(f"root.json écrit dans {args.out} (keyid {kid})")
    return 0


def cmd_sign(args) -> int:
    seed = load_seed(Path(args.key))
    charge = json.loads(Path(args.inp).read_text(encoding="utf-8"))
    signed = charge.get("signed", charge)  # accepte un manifeste ou un bare `signed`
    Path(args.out).write_text(json.dumps(signer_objet(seed, signed), indent=2) + "\n", "utf-8")
    print(f"manifeste signé écrit dans {args.out}")
    return 0


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description="Signature des manifestes de MAJ NovaDesk.")
    sub = p.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("keygen", help="génère une graine Ed25519 (hex).")
    g.add_argument("--out", required=True)
    g.add_argument("--demo", action="store_true", help="graine de démonstration déterministe.")
    g.set_defaults(func=cmd_keygen)

    k = sub.add_parser("pubkey", help="affiche clé publique + keyid.")
    k.add_argument("--key", required=True)
    k.set_defaults(func=cmd_pubkey)

    r = sub.add_parser("emit-root", help="fabrique et signe root.json.")
    r.add_argument("--key", required=True)
    r.add_argument("--out", required=True)
    r.set_defaults(func=cmd_emit_root)

    s = sub.add_parser("sign", help="signe le champ `signed` d'un manifeste.")
    s.add_argument("--key", required=True)
    s.add_argument("--in", dest="inp", required=True)
    s.add_argument("--out", required=True)
    s.set_defaults(func=cmd_sign)

    args = p.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
