#!/usr/bin/env python3
"""Set up a local chain on which the Wallet page's transfers can be tried without money.

Sending from the Wallet page moves real SOL and USDC. To see it do so end to
end with nothing at stake, this prepares a directory for a
`solana-test-validator` of this machine's own: a throwaway key, a "USDC" that
exists only there (at USDC's address, minted by that key, so the program
needs no special case), and a config that points only at that validator.
Then it prints the commands to run.

    python3 scripts/wallet_local.py /tmp/mobius-wallet

Nothing of it reaches mainnet: the validator is a chain of its own on
127.0.0.1. Needs the Solana CLI (solana-test-validator, solana-keygen).
"""
import argparse
import base64
import json
import os
import subprocess

USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"


def mint_account(authority: bytes) -> dict:
    """A token mint of 6 decimals whose mint authority is `authority` (the 82 bytes of the Token program's Mint)."""
    data = (1).to_bytes(4, "little") + authority  # mint authority: Some(key)
    data += (0).to_bytes(8, "little")  # supply
    data += bytes([6, 1])  # decimals, initialized
    data += (0).to_bytes(4, "little") + bytes(32)  # freeze authority: None
    assert len(data) == 82
    return {
        "pubkey": USDC,
        "account": {
            "lamports": 1_461_600,
            "data": [base64.b64encode(data).decode(), "base64"],
            "owner": TOKEN_PROGRAM,
            "executable": False,
            "rentEpoch": 18446744073709551615,
            "space": 82,
        },
    }


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("dir", help="where to put the key, the mint, the config and the ledger")
    p.add_argument("--port", type=int, default=18899, help="the validator's RPC port (default 18899)")
    a = p.parse_args()
    d = os.path.abspath(a.dir)
    os.makedirs(os.path.join(d, "home"), exist_ok=True)
    key = os.path.join(d, "key.json")
    if not os.path.exists(key):
        subprocess.run(["solana-keygen", "new", "--no-bip39-passphrase", "--silent", "-o", key], check=True, stdout=subprocess.DEVNULL)
    os.chmod(key, 0o600)
    wallet = subprocess.run(["solana-keygen", "pubkey", key], check=True, capture_output=True, text=True).stdout.strip()
    with open(key) as f:
        public = bytes(json.load(f)[32:])  # a key file is the secret half, then the public half
    mint = os.path.join(d, "usdc-mint.json")
    with open(mint, "w") as f:
        json.dump(mint_account(public), f)
    url = f"http://127.0.0.1:{a.port}"
    with open(os.path.join(d, "config.toml"), "w") as f:
        f.write(f"""# A LOCAL TEST CHAIN ONLY: solana-test-validator on 127.0.0.1, test SOL, a throwaway key.
[general]
data_dir = "{d}/data"

[venues.solana.rpc]
url = "{url}"
url_env = ""
ws_url_env = ""

[venues.solana.jito]
block_engine_url = "http://127.0.0.1:1"   # nobody there: nothing of this leaves the machine
tip_floor_url = "http://127.0.0.1:1"

[venues.solana.jupiter]
base_url = "http://127.0.0.1:1"           # the session that shows the page quotes nothing

[venues.solana.feeds]
enabled = false                            # and watches no pool: there is none on this chain

[scheduler]
kind = "round_robin"                       # (the default one is driven by the pools)

[execution]
live_enabled = true

[wallet]
keypair_path = "{key}"
pubkey = "{wallet}"
""")
    with open(os.path.join(d, "trade.toml"), "w") as f:
        f.write("""# For the local test chain only: a rule that almost never trades, so that what is
# seen is its budget being set aside and changed (b on the Bots page). It cannot
# swap here (there is no Jupiter on this chain): a raise beyond the wallet's free
# USDC is tried, said to be not sent, and tried again at the next bar.
instrument = "SOL-USDT"
bar = "1m"

[live]
budget_usd = 2.0
stop_total_loss = 0.5
acknowledge = "ALLOW LOSS"

[[experiment]]
name = "dip-test"
rule = "dip"
window = 20
k = 6.0
stop = 0.05
""")
    local = f"NO_PROXY=127.0.0.1 MOBIUS_HOME={d}/home"
    print(f"test wallet {wallet}\n")
    print("1. the validator (leave it running):\n")
    print(f"   solana-test-validator --reset --quiet --bind-address 127.0.0.1 --rpc-port {a.port} "
          f"--faucet-port {a.port + 1001} --ledger {d}/ledger --account {USDC} {mint}\n")
    print("2. the transfers, end to end (test SOL from its faucet, test USDC minted by the key):\n")
    print(f"   NO_PROXY=127.0.0.1 MOBIUS_TEST_VALIDATOR={url} MOBIUS_TEST_KEY={key} \\\n"
          "     cargo test -p mobius-searcher --lib on_a_local_validator_and -- --ignored --nocapture\n")
    print("3. the page itself, from the repository (0 is the Wallet page; s and u send):\n")
    print(f"   {local} mobius-searcher --config {d}/config.toml\n")
    print("4. a test bot whose budget can be changed from the Bots page (9, then b), once step 2 left USDC in the wallet:\n")
    print(f"   {local} mobius-searcher --config {d}/config.toml --trade {d}/trade.toml")


if __name__ == "__main__":
    main()
