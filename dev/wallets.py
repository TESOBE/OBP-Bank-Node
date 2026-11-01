#!/usr/bin/env python3
"""Bank Node wallets on Cardano preprod: create the keys, print the addresses, get test ADA.

    python3 dev/wallets.py              # create missing keys, print addresses, request test ADA
    python3 dev/wallets.py --balance    # show each wallet's balance (Koios, works while Ogmios syncs)

Reads `wallet_skey_path` from dev/node-a.yaml and dev/node-b.yaml. A key that already exists is
never overwritten. A new key is what `cardano-cli address key-gen` writes (a 32-byte Ed25519 seed
in a JSON envelope), and the address is the enterprise address the node derives from it
(crates/obp-blockchain/src/cardano/wallet.rs).

Test ADA: the faucet API needs an API key from IOG (CARDANO_FAUCET_API_KEY). Without one, the
script opens the faucet page, where you paste each address and solve the captcha.
"""
import hashlib
import json
import os
import re
import secrets
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

REPO = Path(__file__).resolve().parent.parent
NODE_CONFIGS = ["dev/node-a.yaml", "dev/node-b.yaml"]
FAUCET_PAGE = "https://docs.cardano.org/cardano-testnets/tools/faucet"
FAUCET_API = "https://faucet.preprod.world.dev.cardano.org/send-money/{address}?api_key={key}"
KOIOS_ADDRESS_INFO = "https://preprod.koios.rest/api/v1/address_info"

BECH32_CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"


def bech32_encode(hrp: str, data: bytes) -> str:
    def polymod(values):
        gen = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3]
        chk = 1
        for v in values:
            top = chk >> 25
            chk = (chk & 0x1FFFFFF) << 5 ^ v
            for i in range(5):
                chk ^= gen[i] if (top >> i) & 1 else 0
        return chk

    acc, bits, words = 0, 0, []
    for b in data:  # 8-bit bytes -> 5-bit words
        acc = (acc << 8) | b
        bits += 8
        while bits >= 5:
            bits -= 5
            words.append((acc >> bits) & 31)
    if bits:
        words.append((acc << (5 - bits)) & 31)
    expanded = [ord(c) >> 5 for c in hrp] + [0] + [ord(c) & 31 for c in hrp]
    pm = polymod(expanded + words + [0] * 6) ^ 1
    checksum = [(pm >> 5 * (5 - i)) & 31 for i in range(6)]
    return hrp + "1" + "".join(BECH32_CHARSET[w] for w in words + checksum)


def enterprise_address(vkey: bytes) -> str:
    """CIP-19 type 6 (payment key hash, no staking part), testnet: header 0x60."""
    key_hash = hashlib.blake2b(vkey, digest_size=28).digest()
    return bech32_encode("addr_test", bytes([0x60]) + key_hash)


def load_seed(skey: Path) -> bytes:
    cbor = bytes.fromhex(json.loads(skey.read_text())["cborHex"])
    if cbor[:2] != b"\x58\x20":
        sys.exit(f"{skey}: expected a 32-byte CBOR byte string")
    return cbor[2:]


def vkey_of(seed: bytes) -> bytes:
    return Ed25519PrivateKey.from_private_bytes(seed).public_key().public_bytes_raw()


def create_key(skey: Path) -> None:
    skey.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    seed = secrets.token_bytes(32)
    envelope = {
        "type": "PaymentSigningKeyShelley_ed25519",
        "description": "Payment Signing Key",
        "cborHex": "5820" + seed.hex(),
    }
    fd = os.open(skey, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(envelope, f, indent=4)
        f.write("\n")


def skey_path(config: str) -> Path:
    match = re.search(r'^\s*wallet_skey_path:\s*"?([^"\s#]+)"?', (REPO / config).read_text(), re.M)
    if not match:
        sys.exit(f"{config}: no wallet_skey_path")
    return (REPO / match.group(1)).resolve()


def wallets() -> list[tuple[str, Path, str]]:
    """(config, skey, address) per node, creating keys that don't exist yet."""
    out = []
    for config in NODE_CONFIGS:
        skey = skey_path(config)
        if skey.exists():
            state = "existing key"
        else:
            create_key(skey)
            state = "NEW key"
        address = enterprise_address(vkey_of(load_seed(skey)))
        skey.with_suffix(".addr").write_text(address + "\n")
        print(f"{config}: {state} {skey.relative_to(REPO)}\n  {address}")
        out.append((config, skey, address))
    return out


def request_faucet(address: str, api_key: str) -> None:
    url = FAUCET_API.format(address=address, key=api_key)
    try:
        with urllib.request.urlopen(urllib.request.Request(url, method="POST"), timeout=30) as r:
            print(f"  faucet: {r.status} {r.read().decode()[:300]}")
    except urllib.error.HTTPError as e:
        print(f"  faucet: {e.code} {e.read().decode()[:300]}")


def show_balances(addresses: list[str]) -> None:
    body = json.dumps({"_addresses": addresses}).encode()
    req = urllib.request.Request(KOIOS_ADDRESS_INFO, data=body,
                                 headers={"content-type": "application/json", "accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        found = {row["address"]: int(row["balance"]) for row in json.load(r)}
    for address in addresses:
        print(f"  {address}: {found.get(address, 0) / 1_000_000:,.6f} tADA")


def main() -> None:
    found = wallets()
    addresses = [a for _, _, a in found]
    if "--balance" in sys.argv:
        print("\nBalances (Koios):")
        show_balances(addresses)
        return
    api_key = os.environ.get("CARDANO_FAUCET_API_KEY")
    if api_key:
        print("\nRequesting test ADA from the faucet API:")
        for address in addresses:
            print(f"  {address}")
            request_faucet(address, api_key)
    else:
        print(f"\nNo CARDANO_FAUCET_API_KEY: paste each address above into the faucet (network Preprod):\n"
              f"  {FAUCET_PAGE}")
        subprocess.run(["xdg-open", FAUCET_PAGE], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print("\nThen check: python3 dev/wallets.py --balance")


if __name__ == "__main__":
    main()
