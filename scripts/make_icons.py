"""Genera el juego de iconos de la app sin depender de Pillow.

Escribe PNG y ICO a mano. El dibujo es un cuadrado redondeado oscuro con unas
barras tipo vumetro, que es lo que hace la app.

    python scripts/make_icons.py
"""

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"

BG = (30, 36, 48, 255)       # azul muy oscuro
BAR = (110, 231, 183, 255)   # verde menta
BAR_DIM = (56, 189, 172, 255)

# Alturas relativas de las barras del vumetro, de izquierda a derecha.
BARS = [0.35, 0.62, 1.00, 0.72, 0.45]


def rounded_alpha(x: int, y: int, size: int, radius: float) -> bool:
    """True si el pixel cae dentro del cuadrado redondeado."""
    r = radius
    cx = min(max(x, r), size - 1 - r)
    cy = min(max(y, r), size - 1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def render(size: int) -> bytes:
    """Devuelve los pixeles RGBA en crudo, fila por fila."""
    radius = size * 0.22
    margin = size * 0.20
    usable = size - 2 * margin
    slot = usable / len(BARS)
    bar_w = slot * 0.52

    rows = bytearray()
    for y in range(size):
        rows.append(0)  # byte de filtro PNG: sin filtro
        for x in range(size):
            if not rounded_alpha(x, y, size, radius):
                rows.extend((0, 0, 0, 0))
                continue

            pixel = BG
            for i, height in enumerate(BARS):
                left = margin + i * slot + (slot - bar_w) / 2
                if not (left <= x < left + bar_w):
                    continue
                bar_h = usable * height
                top = size / 2 - bar_h / 2
                if top <= y < top + bar_h:
                    pixel = BAR if height >= 0.6 else BAR_DIM
                break
            rows.extend(pixel)
    return bytes(rows)


def chunk(tag: bytes, data: bytes) -> bytes:
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def png(size: int) -> bytes:
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # RGBA8
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(render(size), 9))
        + chunk(b"IEND", b"")
    )


def ico(pngs: list[tuple[int, bytes]]) -> bytes:
    """ICO moderno: cada entrada lleva un PNG embebido tal cual."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = len(header) + 16 * len(pngs)
    entries, blobs = b"", b""
    for size, blob in pngs:
        # 256 se codifica como 0 en el directorio del ICO.
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
        blobs += blob
    return header + entries + blobs


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    rendered = {size: png(size) for size in (16, 32, 48, 128, 256, 512)}

    for name, size in [
        ("32x32.png", 32),
        ("128x128.png", 128),
        ("128x128@2x.png", 256),
        ("icon.png", 512),
    ]:
        (OUT / name).write_bytes(rendered[size])
        print(f"{name}  ({len(rendered[size])} bytes)")

    icon_ico = ico([(s, rendered[s]) for s in (16, 32, 48, 128, 256)])
    (OUT / "icon.ico").write_bytes(icon_ico)
    print(f"icon.ico  ({len(icon_ico)} bytes)")


if __name__ == "__main__":
    main()
