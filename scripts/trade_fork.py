#!/usr/bin/env python3
"""Set up a local copy of mainnet on which `--trade` can be run without money.

`--trade` sends real swaps. To see it do so end to end (budget set aside,
buy, sell, stop, close) with nothing at stake, this prepares a directory for
a `solana-test-validator` that holds Jupiter's program and the SOL/USDC pools
of one DEX, copied from mainnet as they are now, a throwaway key, a config
that points only at that validator, and a rules file that trades every few
minutes. Then it prints the three commands to run.

    python3 scripts/trade_fork.py /tmp/mobius-fork

The copy is frozen at the moment it is made while Jupiter goes on quoting the
live pools, so the rules file allows 3 % of slippage, and a quote that names
an account that was not copied is refused by the validator (the runner says
so and tries again at the next bar). An hour or two is what a copy is good
for. Needs the Solana CLI (solana-test-validator, solana-keygen).
"""
import argparse
import json
import os
import subprocess
import time
import urllib.parse
import urllib.request

SOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
MAINNET = "https://api.mainnet-beta.solana.com"
TIP_ACCOUNT = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"  # the runner's first Jito tip account
UPGRADEABLE = "BPFLoaderUpgradeab1e11111111111111111111111"
# what the validator has without copying: built-in programs, sysvars, the token programs
BUILT_IN_OWNERS = {"NativeLoader1111111111111111111111111111111", "Sysvar1111111111111111111111111111111111111"}
BUILT_IN = {
    "11111111111111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
}
HEADERS = {"User-Agent": "mobius-trade-fork", "Accept": "application/json", "content-type": "application/json"}


def get(url):
    return json.load(urllib.request.urlopen(urllib.request.Request(url, headers=HEADERS), timeout=30))


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    return json.load(urllib.request.urlopen(urllib.request.Request(MAINNET, data=body, headers=HEADERS), timeout=30))["result"]


def accounts_of_a_swap(taker, dex, rounds):
    """Every account the swaps of `dex` name, both ways, over a few quotes."""
    seen, routes = set(), set()
    for _ in range(rounds):
        for inp, out, amount in ((SOL, USDC, 16_500_000), (USDC, SOL, 2_000_000)):
            q = dict(inputMint=inp, outputMint=out, amount=amount, taker=taker, slippageBps=300,
                     computeUnitPricePercentile="high", blockhashSlotsToExpiry=150, wrapAndUnwrapSol="true",
                     dexes=dex, maxAccounts=30, forJitoBundle="true")
            r = get("https://api.jup.ag/swap/v2/build?" + urllib.parse.urlencode(q))
            routes.add(" + ".join(f"{s['swapInfo']['label']} {s['swapInfo']['ammKey'][:6]}" for s in r["routePlan"]))
            ixs = (r.get("setupInstructions", []) + [r["swapInstruction"]] + r.get("otherInstructions", [])
                   + ([r["cleanupInstruction"]] if r.get("cleanupInstruction") else []))
            for ix in ixs:
                seen.add(ix["programId"])
                seen.update(a["pubkey"] for a in ix["accounts"])
            seen.update(r.get("addressesByLookupTableAddress") or {})
            time.sleep(2.5)  # keyless Jupiter: one request in two seconds
    return sorted(seen), sorted(routes)


def clone_arguments(keys):
    info = rpc("getMultipleAccounts", [keys, {"encoding": "base64", "dataSlice": {"offset": 0, "length": 0}}])["value"]
    args = []
    for key, account in zip(keys, info):
        if account is None or key in BUILT_IN or account["owner"] in BUILT_IN_OWNERS:
            continue  # the test wallet's own accounts are not on mainnet
        upgradeable = account["executable"] and account["owner"] == UPGRADEABLE
        args += ["--clone-upgradeable-program" if upgradeable else "--clone", key]
    return args


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("dir", help="where the key, the config, the rules file and the ledger go")
    p.add_argument("--dex", default="Meteora DLMM", help="the one DEX whose pools are copied (Jupiter's name)")
    p.add_argument("--port", type=int, default=18899)
    a = p.parse_args()
    d = os.path.abspath(a.dir)
    os.makedirs(os.path.join(d, "home"), exist_ok=True)
    key = os.path.join(d, "key.json")
    if not os.path.exists(key):
        subprocess.run(["solana-keygen", "new", "--no-bip39-passphrase", "--silent", "-o", key], check=True, stdout=subprocess.DEVNULL)
    os.chmod(key, 0o600)
    taker = subprocess.run(["solana-keygen", "pubkey", key], check=True, capture_output=True, text=True).stdout.strip()
    keys, routes = accounts_of_a_swap(taker, a.dex, rounds=3)
    args = clone_arguments(keys)
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

[execution]
live_enabled = true

[wallet]
keypair_path = "{key}"
pubkey = "{taker}"
""")
    with open(os.path.join(d, "trade.toml"), "w") as f:
        f.write(f"""# For the local copy only: a rule that trades every few minutes, to see every path.
instrument = "SOL-USDT"
bar = "1m"

[live]
budget_usd = 2.0
stop_total_loss = 0.5
slippage_bps = 300        # the copy is frozen, Jupiter quotes the live pools
acknowledge = "ALLOW LOSS"
dexes = ["{a.dex}"]

[[experiment]]
name = "reversal"
rule = "sign-reversal"
""")
    local = f"NO_PROXY=127.0.0.1 MOBIUS_HOME={d}/home"
    print(f"test wallet {taker}; routes seen: {', '.join(routes)}; {len(args) // 2} accounts to copy\n")
    print("1. the validator (leave it running):\n")
    print(f"   solana-test-validator --reset --quiet --bind-address 127.0.0.1 --rpc-port {a.port} "
          f"--faucet-port {a.port + 1001} --ledger {d}/ledger --url {MAINNET} {' '.join(args)}\n")
    print("2. test SOL for the wallet, and for the tip account (on mainnet it has a balance):\n")
    print(f"   NO_PROXY=127.0.0.1 solana airdrop 2 {taker} --url {url} && NO_PROXY=127.0.0.1 solana airdrop 1 {TIP_ACCOUNT} --url {url}\n")
    print("3. the runner, from the repository (add --dry-run first, --close to end; --lab-report for the report):\n")
    print(f"   {local} mobius-searcher --config {d}/config.toml --trade {d}/trade.toml")


if __name__ == "__main__":
    main()
