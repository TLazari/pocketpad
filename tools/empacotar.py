"""Monta o .hnmod (o pacote do registro do Horizon) a partir de mixins/pocketpad.

    python tools/empacotar.py          # -> dist/pocketpad-<versão>.hnmod

O .hnmod é um zip com o código-fonte: `horizon-native.json` e, em
`mixins/<id>/`, só arquivos `.rs` e `.toml` (as regras de
`horizon_mods::index::native::Bundle::read`). Antes, rode
`python tools/pagina.py embutir` para a página do celular ir junto no runtime.rs.
"""
import hashlib
import json
import sys
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MOD = ROOT / "mixins" / "pocketpad"
# O horizon-mixins com que o launcher aplica os mixins (`native_mods::MIXINS_REV`
# do launcher 0.2.28) e a versão do Minecraft da revisão fixada.
MIXINS = "3f99a5c60dfa311cc365524c44ae2fd799bfa1de"
GAME = "26.3"


def main() -> None:
    toml = tomllib.loads((MOD / "horizon-mixin.toml").read_text(encoding="utf-8"))
    mod_id, version = toml["mod"]["id"], toml["mod"]["version"]
    horizon = toml["compatibility"]["horizon_revision"]
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    if cargo["package"]["version"] != version:
        sys.exit(f"Cargo.toml diz {cargo['package']['version']}, horizon-mixin.toml diz {version}")
    if MOD.name != mod_id:
        sys.exit(f"a pasta precisa se chamar mixins/{mod_id}")
    manifest = {"schema": 1, "id": mod_id, "version": version, "game": GAME, "horizon": horizon, "mixins": MIXINS}
    files = sorted(p for p in MOD.iterdir() if p.is_file() and p.suffix in (".rs", ".toml"))
    out = ROOT / "dist" / f"{mod_id}-{version}.hnmod"
    out.parent.mkdir(exist_ok=True)
    # Datas fixas: o mesmo código dá sempre o mesmo arquivo (e o mesmo SHA-256).
    stamp = (2026, 1, 1, 0, 0, 0)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        info = zipfile.ZipInfo("horizon-native.json", stamp)
        info.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(info, json.dumps(manifest, indent=2) + "\n")
        for p in files:
            info = zipfile.ZipInfo(f"mixins/{mod_id}/{p.name}", stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            z.writestr(info, p.read_bytes())
    digest = hashlib.sha256(out.read_bytes()).hexdigest()
    print(f"{out.relative_to(ROOT)}  {out.stat().st_size} bytes")
    print(f"sha256 {digest}")
    for p in files:
        print(f"  mixins/{mod_id}/{p.name}")


if __name__ == "__main__":
    main()
