import argparse
import asyncio
import struct

from pymodbus.datastore import ModbusSequentialDataBlock, ModbusServerContext
from pymodbus.server import StartAsyncTcpServer

try:
    from pymodbus.datastore import ModbusDeviceContext as DeviceContext

    CONTEXT_KEY = "devices"
except ImportError:
    from pymodbus.datastore import ModbusSlaveContext as DeviceContext

    CONTEXT_KEY = "slaves"


def block(values):
    return ModbusSequentialDataBlock(1, values)


def main():
    parser = argparse.ArgumentParser(description="Modbus TCP 测试服务端")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=1502)
    parser.add_argument("--size", type=int, default=1000)
    args = parser.parse_args()

    hr = [0] * args.size
    hr[20], hr[21] = struct.unpack(">HH", struct.pack(">f", 12.5))
    ir = [0] * args.size
    ir[5] = 42
    device = DeviceContext(
        di=block([0] * args.size),
        co=block([0] * args.size),
        hr=block(hr),
        ir=block(ir),
    )
    context = ModbusServerContext(**{CONTEXT_KEY: device}, single=True)
    print(f"Modbus TCP 测试服务端 {args.host}:{args.port}", flush=True)
    asyncio.run(StartAsyncTcpServer(context=context, address=(args.host, args.port)))


if __name__ == "__main__":
    main()
