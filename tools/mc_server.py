import argparse
import socket
import struct
import threading

WORD_DEVICES = {0xA8: "D", 0xB4: "W", 0xAF: "R", 0xB0: "ZR", 0xA9: "SD", 0xC2: "TN", 0xC5: "CN", 0xCC: "Z"}
BIT_DEVICES = {
    0x9C: "X", 0x9D: "Y", 0x90: "M", 0x92: "L", 0x93: "F", 0x94: "V",
    0xA0: "B", 0x91: "SM", 0xA1: "SB", 0xC1: "TS", 0xC4: "CS",
}

memory = {}
lock = threading.Lock()
verbose = False


def get(code, n):
    return memory.setdefault(code, {}).get(n, 0)


def put(code, n, v):
    memory.setdefault(code, {})[n] = v


def process(body):
    _timer, cmd, sub = struct.unpack("<HHH", body[:6])
    start = int.from_bytes(body[6:9], "little")
    code, count = body[9], struct.unpack("<H", body[10:12])[0]
    data = body[12:]
    if code not in WORD_DEVICES and code not in BIT_DEVICES:
        return 0xC056, b""
    with lock:
        if cmd == 0x0401 and sub == 0x0000:
            return 0, b"".join(struct.pack("<H", get(code, start + i)) for i in range(count))
        if cmd == 0x0401 and sub == 0x0001:
            bits = [get(code, start + i) & 1 for i in range(count)] + [0]
            return 0, bytes((bits[i] << 4) | bits[i + 1] for i in range(0, count, 2))
        if cmd == 0x1401 and sub == 0x0000:
            for i in range(count):
                put(code, start + i, struct.unpack("<H", data[2 * i : 2 * i + 2])[0])
            return 0, b""
        if cmd == 0x1401 and sub == 0x0001:
            for i in range(count):
                put(code, start + i, (data[i // 2] >> (4 if i % 2 == 0 else 0)) & 1)
            return 0, b""
    return 0xC059, b""


def handle(conn):
    with conn:
        while True:
            head = conn.recv(9, socket.MSG_WAITALL)
            if len(head) < 9:
                return
            if head[:2] != b"\x50\x00":
                print("非 3E 二进制请求帧，断开连接", flush=True)
                return
            length = struct.unpack("<H", head[7:9])[0]
            body = conn.recv(length, socket.MSG_WAITALL)
            end, payload = process(body)
            resp = struct.pack("<H", end) + payload
            conn.sendall(b"\xD0\x00" + head[2:7] + struct.pack("<H", len(resp)) + resp)
            if verbose:
                cmd, sub = struct.unpack("<HH", body[2:6])
                name = WORD_DEVICES.get(body[9]) or BIT_DEVICES.get(body[9], hex(body[9]))
                start = int.from_bytes(body[6:9], "little")
                count = struct.unpack("<H", body[10:12])[0]
                print(f"cmd=0x{cmd:04X} sub={sub} {name}{start} x{count} end=0x{end:04X}", flush=True)


def main():
    global verbose
    parser = argparse.ArgumentParser(description="三菱 MC 协议 3E 二进制帧测试服务端（批量读写 0401/1401）")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=5007)
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    verbose = args.verbose

    put(0xA8, 100, 0x0000)
    put(0xA8, 101, 0x4148)
    put(0x9C, 0x1F, 1)

    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((args.host, args.port))
    server.listen()
    print(f"MC 3E 测试服务端 {args.host}:{args.port}", flush=True)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
