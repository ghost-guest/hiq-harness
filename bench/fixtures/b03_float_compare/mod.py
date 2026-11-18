import struct

def total(prices):
    s = 0.0
    for p in prices:
        s = struct.unpack('f', struct.pack('f', s + p))[0]
    return s
