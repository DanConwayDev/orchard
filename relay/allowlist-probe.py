#!/usr/bin/env python3
# Admission probe for an Orchard relay: verifies the kind allowlist is
# enforcing and that Orchard-shaped reads still work. Standard library
# only, no dependencies to install on a fresh server.
#
#   python3 allowlist-probe.py ws://127.0.0.1:8008
#   python3 allowlist-probe.py wss://relay.example.com
#
# Three checks, with expected results:
#   1. a validly signed kind 1 is rejected ("event kind is blocked by
#      relay")
#   2. a validly signed kind 30066 (migration, throwaway app id) is
#      admitted; it lands on the relay but is inert (clients trim
#      migrations that reference nothing) and expires: the event
#      carries a NIP-40 expiration tag a few minutes out, so the
#      relay stops serving it and purges it without operator help
#   3. a kind-based REQ (the exact shape limit_scrapers=true would
#      reject) returns events or EOSE
#
# The signing leg is BIP-340 per the spec reference. The relay is the
# validator: the OK-true on check 2 proves the signatures are good, so
# the rejection on check 1 is attributable to the allowlist and not to
# a broken signer. Exit code is 0 only when all three checks pass.

import base64
import hashlib
import json
import os
import socket
import ssl
import sys
import time
from urllib.parse import urlparse

p = 2**256 - 2**32 - 977
n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
G = (0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798,
     0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8)


def pt_add(P1, P2):
    if P1 is None:
        return P2
    if P2 is None:
        return P1
    if P1[0] == P2[0] and P1[1] != P2[1]:
        return None
    if P1 == P2:
        lam = (3 * P1[0] * P1[0] * pow(2 * P1[1], -1, p)) % p
    else:
        lam = ((P2[1] - P1[1]) * pow(P2[0] - P1[0], -1, p)) % p
    x3 = (lam * lam - P1[0] - P2[0]) % p
    return (x3, (lam * (P1[0] - x3) - P1[1]) % p)


def pt_mul(k, P):
    R = None
    for i in range(256):
        if (k >> i) & 1:
            R = pt_add(R, P)
        P = pt_add(P, P)
    return R


def tagged_hash(tag, msg):
    h = hashlib.sha256(tag.encode()).digest()
    return hashlib.sha256(h + h + msg).digest()


def schnorr(msg32, sk):
    d0 = sk % n
    P = pt_mul(d0, G)
    d = (n - d0) if P[1] % 2 else d0
    t = bytes(a ^ b for a, b in zip(d.to_bytes(32, "big"),
                                     tagged_hash("BIP0340/aux", os.urandom(32))))
    k0 = int.from_bytes(
        tagged_hash("BIP0340/nonce", t + P[0].to_bytes(32, "big") + msg32), "big") % n
    R = pt_mul(k0, G)
    k = (n - k0) if R[1] % 2 else k0
    rx = R[0].to_bytes(32, "big")
    e = int.from_bytes(
        tagged_hash("BIP0340/challenge", rx + P[0].to_bytes(32, "big") + msg32), "big") % n
    return rx + ((k + e * d) % n).to_bytes(32, "big")


def make_event(kind, content, tags, sk):
    P = pt_mul(sk, G)
    pub = P[0].to_bytes(32, "big").hex()
    created = int(time.time())
    ser = json.dumps([0, pub, created, kind, tags, content],
                     separators=(",", ":")).encode()
    eid = hashlib.sha256(ser).digest()
    return {"id": eid.hex(), "pubkey": pub, "created_at": created,
            "kind": kind, "tags": tags, "content": content,
            "sig": schnorr(eid, sk).hex()}


def connect(url):
    parsed = urlparse(url)
    tls = parsed.scheme == "wss"
    host = parsed.hostname
    if host is None:
        raise SystemExit(f"not a relay URL: {url}")
    port = parsed.port or (443 if tls else 80)
    path = parsed.path or "/"
    s = socket.create_connection((host, port), timeout=15)
    if tls:
        s = ssl.create_default_context().wrap_socket(s, server_hostname=host)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
               f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {key}\r\n"
               f"Sec-WebSocket-Version: 13\r\n\r\n").encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(1024)
        if not chunk:
            raise ConnectionError("closed during handshake")
        buf += chunk
    head, _, leftover = buf.partition(b"\r\n\r\n")
    if b"101" not in head.split(b"\r\n")[0]:
        raise ConnectionError(f"no 101 switch: {head[:100]!r}")
    return s, bytearray(leftover)


def send(s, obj):
    payload = json.dumps(obj, separators=(",", ":")).encode()
    mask = os.urandom(4)
    hdr = bytes([0x81])
    length = len(payload)
    if length < 126:
        hdr += bytes([0x80 | length])
    else:
        hdr += bytes([0x80 | 126]) + length.to_bytes(2, "big")
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))


def recv(s, leftover):
    def rd(k):
        while len(leftover) < k:
            chunk = s.recv(k - len(leftover))
            if not chunk:
                raise ConnectionError("closed mid-frame")
            leftover.extend(chunk)
        out = bytes(leftover[:k])
        del leftover[:k]
        return out
    while True:
        b1, b2 = rd(1)[0], rd(1)[0]
        length = b2 & 0x7F
        if length == 126:
            length = int.from_bytes(rd(2), "big")
        elif length == 127:
            length = int.from_bytes(rd(8), "big")
        payload = rd(length) if length else b""
        op = b1 & 0x0F
        if op == 0x1:
            return payload.decode()
        if op == 0x8:
            raise ConnectionError("closed by relay")


def main():
    url = sys.argv[1] if len(sys.argv) > 1 else "ws://127.0.0.1:8008"
    sk = (int.from_bytes(os.urandom(32), "big") % (n - 1)) + 1
    s, leftover = connect(url)
    ok = True

    print(f"== probe {url}")
    print("== 1. kind 1 (foreign): expect rejection by the allowlist")
    send(s, ["EVENT", make_event(1, "allowlist probe: foreign kind", [], sk)])
    reply = json.loads(recv(s, leftover))
    print("    " + json.dumps(reply)[:140])
    ok &= reply[0] == "OK" and reply[2] is False

    print("== 2. kind 30066 (Orchard kind): expect OK true")
    # NIP-40 expiration a few minutes out: the admission proof serves
    # its purpose and then the relay stops serving it, so probing a
    # public relay leaves nothing behind (verified against 0.10.0)
    expires = int(time.time()) + 300
    send(s, ["EVENT", make_event(30066, "",
                                 [["d", "com.probe.AllowlistTest"],
                                  ["expiration", str(expires)]], sk)])
    reply = json.loads(recv(s, leftover))
    print("    " + json.dumps(reply)[:140])
    ok &= reply[0] == "OK" and reply[2] is True

    print("== 3. kind-based REQ: expect events then EOSE")
    send(s, ["REQ", "probe", {"kinds": [32267], "limit": 5}])
    count = 0
    saw_eose = False
    for _ in range(50):
        msg = json.loads(recv(s, leftover))
        if msg[0] == "EVENT":
            count += 1
            if count == 1:
                d = next((t[1] for t in msg[2]["tags"] if t[0] == "d"), "?")
                print(f"    first event: kind {msg[2]['kind']} d={d}")
        elif msg[0] == "EOSE":
            saw_eose = True
            print(f"    EOSE after {count} listing event(s)")
            break
        else:
            print("    " + json.dumps(msg)[:140])
    send(s, ["CLOSE", "probe"])
    s.close()
    ok &= saw_eose

    print("PASS" if ok else "FAIL", flush=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
