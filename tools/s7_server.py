import argparse
import struct
import time

import snap7
from snap7.type import SrvArea


def main():
    parser = argparse.ArgumentParser(description="S7 测试服务端（基于 python-snap7）")
    parser.add_argument("--port", type=int, default=1102)
    parser.add_argument("--db-size", type=int, default=256)
    parser.add_argument("--dump", action="store_true", help="每秒打印 DB1 与 M 区前 32 字节")
    args = parser.parse_args()

    db1 = bytearray(args.db_size)
    db2 = bytearray(args.db_size)
    mk = bytearray(256)
    struct.pack_into(">h", db1, 4, -7)
    struct.pack_into(">f", db1, 20, 12.5)
    mk[10] = 0b100

    server = snap7.Server()
    server.register_area(SrvArea.DB, 1, db1)
    server.register_area(SrvArea.DB, 2, db2)
    server.register_area(SrvArea.MK, 0, mk)
    server.start(tcp_port=args.port)
    print(f"S7 测试服务端 0.0.0.0:{args.port}（DB1、DB2、M 区；机架 0 插槽 1）", flush=True)
    print("注意：python-snap7 服务端的单 BIT 写入会忽略位偏移，真实 PLC 无此问题", flush=True)
    try:
        while True:
            time.sleep(1)
            if args.dump:
                print("DB1", db1[:32].hex(" "), "| M", mk[:32].hex(" "), flush=True)
    except KeyboardInterrupt:
        server.stop()


if __name__ == "__main__":
    main()
