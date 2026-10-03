#!/usr/bin/env python3
"""Ground truth for the local pool math (crates/market/src/amm).

For a pool, a direction and an amount: read the pool's accounts, simulate the
pool program's own swap instruction on mainnet (as a funded public account,
signatures not verified, nothing sent), and read the accounts again. When the
pool did not change in between, the simulated output is exactly what that
state returns for that input. Samples are appended to a fixture that the
Rust tests replay.

    python3 scripts/amm_parity.py whirlpool|raydium|dlmm  [samples]  [fixture.json]

Standard library only. Uses the public RPC unless SOLANA_RPC_URL is set.
"""
import base64, hashlib, json, os, struct, sys, time, urllib.request

RPC = os.environ.get("SOLANA_RPC_URL", "https://api.mainnet-beta.solana.com")
# simulation only: a public account that holds SOL and USDC
TAKER = "F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe"
SOL, USDC = "So11111111111111111111111111111111111111112", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
TOKEN, ATA = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
SYSTEM = "11111111111111111111111111111111"
POOLS = {
    "whirlpool": ("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
    "raydium": ("CYbD9RaToYMtWKA7QZyoLahnHdWq553Vm62Lh6qWtuxq", "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
    "dlmm": ("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR", "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58d(s):
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    return n.to_bytes(32, "big")


def b58_bytes(s):
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    return bytes(len(s) - len(s.lstrip("1"))) + n.to_bytes((n.bit_length() + 7) // 8, "big")


def b58e(b):
    n, s = int.from_bytes(b, "big"), ""
    while n:
        n, r = divmod(n, 58)
        s = B58[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s


P = 2**255 - 19
D = (-121665 * pow(121666, P - 2, P)) % P


def on_curve(b):
    y = int.from_bytes(b, "little") & ((1 << 255) - 1)
    if y >= P:
        return False
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P:
        x = x * pow(2, (P - 1) // 4, P) % P
    return (x * x - x2) % P == 0


def pda(seeds, program):
    for bump in range(255, -1, -1):
        h = hashlib.sha256(b"".join(seeds) + bytes([bump]) + b58d(program) + b"ProgramDerivedAddress").digest()
        if not on_curve(h):
            return b58e(h)


def ata(owner, mint):
    return pda([b58d(owner), b58d(TOKEN), b58d(mint)], ATA)


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for attempt in range(5):
        try:
            req = urllib.request.Request(RPC, body, {"content-type": "application/json"})
            r = json.loads(urllib.request.urlopen(req, timeout=30).read())
            if "error" in r and r["error"].get("code") == 429:
                raise OSError("rate limited")
            return r
        except OSError as e:
            time.sleep(2 + 3 * attempt)
    raise SystemExit(f"{method}: no answer from {RPC}")


def accounts(addresses):
    r = rpc("getMultipleAccounts", [addresses, {"encoding": "base64", "commitment": "confirmed"}])["result"]
    return r["context"]["slot"], [base64.b64decode(v["data"][0]) if v else None for v in r["value"]]


def compact(n):
    out = b""
    while True:
        if n < 0x80:
            return out + bytes([n])
        out += bytes([n & 0x7F | 0x80])
        n >>= 7


def transaction(payer, instructions):
    """A legacy transaction with an empty signature. instructions: (program, [(key, signer, writable)], data)."""
    metas = {payer: (True, True)}
    for program, keys, _ in instructions:
        for key, signer, writable in keys:
            s, w = metas.get(key, (False, False))
            metas[key] = (s or signer, w or writable)
        metas.setdefault(program, (False, False))
    order = sorted(metas, key=lambda k: (not metas[k][0], not metas[k][1], k != payer))
    signers = sum(metas[k][0] for k in order)
    ro_signed = sum(metas[k][0] and not metas[k][1] for k in order)
    ro_unsigned = sum(not metas[k][0] and not metas[k][1] for k in order)
    msg = bytes([signers, ro_signed, ro_unsigned]) + compact(len(order)) + b"".join(b58d(k) for k in order) + bytes(32)
    msg += compact(len(instructions))
    for program, keys, data in instructions:
        msg += bytes([order.index(program)]) + compact(len(keys)) + bytes(order.index(k) for k, _, _ in keys)
        msg += compact(len(data)) + data
    return base64.b64encode(compact(signers) + bytes(64) * signers + msg).decode(), order


def simulate(instructions, vault):
    """Simulate; returns (slot, error, logs, tokens the program paid out of `vault`)."""
    tx, order = transaction(TAKER, instructions)
    cfg = {"sigVerify": False, "replaceRecentBlockhash": True, "encoding": "base64", "commitment": "confirmed",
           "innerInstructions": True}
    r = rpc("simulateTransaction", [tx, cfg])
    if "error" in r:
        raise SystemExit(f"simulateTransaction: {r['error']}")
    v = r["result"]["value"]
    paid = 0
    for group in v.get("innerInstructions") or []:
        for ix in group["instructions"]:
            # token transfers out of the vault; the node answers parsed or raw
            if "parsed" in ix:
                info = ix["parsed"].get("info", {})
                if ix.get("programId") == TOKEN and info.get("source") == vault:
                    paid += int(info.get("amount") or info.get("tokenAmount", {}).get("amount", 0))
                continue
            # raw: the program and accounts as addresses, or as indices into the message
            program = ix.get("programId") or order[ix["programIdIndex"]]
            first = ix["accounts"][0] if ix["accounts"] else None
            source = first if isinstance(first, str) else order[first] if first is not None else None
            data = b58_bytes(ix["data"])
            if program == TOKEN and data[:1] in (b"\x03", b"\x0c") and source == vault:
                paid += struct.unpack_from("<Q", data, 1)[0]
    return r["result"]["context"]["slot"], v.get("err"), v.get("logs") or [], paid


def disc(name):
    return hashlib.sha256(f"global:{name}".encode()).digest()[:8]


def fund_wsol(lamports):
    """Instructions that leave `lamports` of wrapped SOL in the taker's token account."""
    w = ata(TAKER, SOL)
    create = (ATA, [(TAKER, True, True), (w, False, True), (TAKER, False, False), (SOL, False, False),
                    (SYSTEM, False, False), (TOKEN, False, False)], bytes([1]))
    out = [create]
    if lamports:
        out.append((SYSTEM, [(TAKER, True, True), (w, False, True)], struct.pack("<IQ", 2, lamports)))
        out.append((TOKEN, [(w, False, True)], bytes([17])))  # SyncNative
    return out


# ───────────────────────────── Orca Whirlpool ─────────────────────────────
def whirlpool_state(pool, program):
    _, (d,) = accounts([pool])
    spacing, tick = struct.unpack_from("<H", d, 41)[0], struct.unpack_from("<i", d, 81)[0]
    span = 88 * spacing
    start = tick // span * span
    arrays = [pda([b"tick_array", b58d(pool), str(start + k * span).encode()], program) for k in (-2, -1, 0, 1, 2)]
    return [pool] + arrays, {"vault_a": b58e(d[133:165]), "vault_b": b58e(d[213:245]), "start": start, "span": span}


def whirlpool_swap(pool, program, info, arrays, a_to_b, amount):
    # the array holding the current tick, then two further in the swap's direction
    i = 3  # arrays = [-2, -1, 0, +1, +2] after the pool
    seq = [arrays[i], arrays[i - 1], arrays[i - 2]] if a_to_b else [arrays[i], arrays[i + 1], arrays[i + 2]]
    limit = 4295048016 if a_to_b else 79226673515401279992447579055
    data = disc("swap") + struct.pack("<QQ", amount, 0) + limit.to_bytes(16, "little") + bytes([1, a_to_b])
    keys = [(TOKEN, False, False), (TAKER, True, False), (pool, False, True),
            (ata(TAKER, SOL), False, True), (info["vault_a"], False, True),
            (ata(TAKER, USDC), False, True), (info["vault_b"], False, True)]
    keys += [(a, False, True) for a in seq] + [(pda([b"oracle", b58d(pool)], program), False, True)]
    return (program, keys, data), info["vault_b" if a_to_b else "vault_a"]


# ───────────────────────────── Raydium CLMM ───────────────────────────────
def raydium_state(pool, program):
    _, (d,) = accounts([pool])
    spacing, tick = struct.unpack_from("<H", d, 235)[0], struct.unpack_from("<i", d, 269)[0]
    span = 60 * spacing
    start = tick // span * span
    arrays = [pda([b"tick_array", b58d(pool), struct.pack(">i", start + k * span)], program) for k in (-2, -1, 0, 1, 2)]
    info = {"config": b58e(d[9:41]), "vault_a": b58e(d[137:169]), "vault_b": b58e(d[169:201]),
            "observation": b58e(d[201:233]), "start": start, "span": span}
    return [pool, info["config"]] + arrays, info


def raydium_swap(pool, program, info, arrays, a_to_b, amount):
    i = 4  # arrays = [-2, -1, 0, +1, +2] after pool and config
    seq = [arrays[i], arrays[i - 1], arrays[i - 2]] if a_to_b else [arrays[i], arrays[i + 1], arrays[i + 2]]
    ins, outs = (SOL, USDC) if a_to_b else (USDC, SOL)
    vin, vout = (info["vault_a"], info["vault_b"]) if a_to_b else (info["vault_b"], info["vault_a"])
    data = disc("swap") + struct.pack("<QQ", amount, 0) + (0).to_bytes(16, "little") + bytes([1])
    keys = [(TAKER, True, False), (info["config"], False, False), (pool, False, True),
            (ata(TAKER, ins), False, True), (ata(TAKER, outs), False, True), (vin, False, True), (vout, False, True),
            (info["observation"], False, True), (TOKEN, False, False)]
    keys += [(a, False, True) for a in seq]
    return (program, keys, data), vout


# ───────────────────────────── Meteora DLMM ───────────────────────────────
def dlmm_state(pool, program):
    _, (d,) = accounts([pool])
    active = struct.unpack_from("<i", d, 76)[0]
    idx = active // 70
    arrays = [pda([b"bin_array", b58d(pool), struct.pack("<q", idx + k)], program) for k in (-2, -1, 0, 1, 2)]
    info = {"reserve_x": b58e(d[152:184]), "reserve_y": b58e(d[184:216]), "oracle": b58e(d[552:584]), "index": idx}
    return [pool] + arrays, info


def dlmm_swap(pool, program, info, arrays, a_to_b, amount):
    i = 3
    seq = [arrays[i], arrays[i - 1], arrays[i - 2]] if a_to_b else [arrays[i], arrays[i + 1], arrays[i + 2]]
    ins, outs = (SOL, USDC) if a_to_b else (USDC, SOL)
    data = disc("swap") + struct.pack("<QQ", amount, 0)
    keys = [(pool, False, True), (program, False, False),  # no bitmap extension
            (info["reserve_x"], False, True), (info["reserve_y"], False, True),
            (ata(TAKER, ins), False, True), (ata(TAKER, outs), False, True),
            (SOL, False, False), (USDC, False, False), (info["oracle"], False, True),
            (program, False, False),  # no host fee account
            (TAKER, True, False), (TOKEN, False, False), (TOKEN, False, False),
            (pda([b"__event_authority"], program), False, False), (program, False, False)]
    keys += [(a, False, True) for a in seq]
    return (program, keys, data), info["reserve_y" if a_to_b else "reserve_x"]


KINDS = {"whirlpool": (whirlpool_state, whirlpool_swap), "raydium": (raydium_state, raydium_swap),
         "dlmm": (dlmm_state, dlmm_swap)}
# SOL in (lamports) and USDC in (millionths): small, the bot's sizes, and large enough to cross ticks
SIZES = {True: [1_000_000, 100_000_000, 1_000_000_000, 20_000_000_000, 150_000_000_000],
         False: [150_000, 12_000_000, 120_000_000, 2_400_000_000, 18_000_000_000]}


CLOCK = "SysvarC1ock11111111111111111111111111111111"


def sample(kind, a_to_b, amount, blobs):
    pool, program = POOLS[kind]
    state, swap = KINDS[kind]
    addresses, info = state(pool, program)
    _, before = accounts(addresses + [CLOCK])
    ix, vault = swap(pool, program, info, addresses, a_to_b, amount)
    slot, err, logs, paid = simulate(fund_wsol(amount if a_to_b else 0) + [ix], vault)
    _, after = accounts(addresses)
    if err:
        return {"kind": kind, "a_to_b": a_to_b, "amount_in": amount, "err": json.dumps(err), "logs": logs[-6:]}
    if before[: len(addresses)] != after:
        return None  # the pool moved while we looked: not a clean sample
    refs = {}
    for address, data in zip(addresses, before):
        if data is not None:
            key = hashlib.sha256(data).hexdigest()[:16]
            blobs[key] = base64.b64encode(data).decode()
            refs[address] = key
    return {
        "kind": kind, "pool": pool, "a_to_b": a_to_b, "amount_in": amount,
        # what the pool's vault paid out in the simulated swap
        "amount_out": paid,
        # the chain's clock when the accounts were read (time-dependent fees)
        "slot": slot, "unix_time": struct.unpack_from("<q", before[-1], 32)[0],
        "accounts": refs,
    }


def main():
    kind = sys.argv[1]
    want = int(sys.argv[2]) if len(sys.argv) > 2 else 4
    path = sys.argv[3] if len(sys.argv) > 3 else f"fixtures/amm/{kind}.json"
    # accounts are stored once, by content: tick arrays rarely change between samples
    fixture = json.load(open(path)) if os.path.exists(path) else {"blobs": {}, "samples": []}
    samples, blobs = fixture["samples"], fixture["blobs"]
    got = moved = failed = 0
    plan = [(d, a) for a_i in range(5) for d in (True, False) for a in [SIZES[d][a_i]]]
    for a_to_b, amount in (plan * want)[:want]:
        s = sample(kind, a_to_b, amount, blobs)
        if s is None:
            moved += 1
            continue
        if "err" in s:
            # e.g. a swap too large for the three tick arrays the instruction carries
            print(f"{kind} {'A→B' if a_to_b else 'B→A'} in {amount:>14} failed: {s['err']} · {s['logs'][-3:]}")
            failed += 1
            continue
        samples.append(s)
        got += 1
        print(f"{kind} {'A→B' if a_to_b else 'B→A'} in {amount:>14} out {s['amount_out']:>14} slot {s['slot']}")
        time.sleep(1.5)
    json.dump(fixture, open(path, "w"), separators=(",", ":"))
    print(f"{got} samples added ({moved} dropped: the pool changed during the sample, {failed} failed); "
          f"{len(samples)} in {path}")


if __name__ == "__main__":
    main()
