"""A página do controle sem recompilar o jogo.

    python tools/pagina.py extrair   # runtime.rs (constante PAGE) -> pagina/pocketpad.html
    python tools/pagina.py instalar  # pagina/ -> <pasta do jogo>/config/pocketpad.html (vale na hora)
    python tools/pagina.py embutir   # pagina/ -> constante PAGE do runtime.rs (antes de um native-build)
    python tools/pagina.py remover   # tira a cópia do jogo: volta a valer a página embutida

Com a cópia instalada, o mod serve ela no lugar da embutida e relê a cada
abertura: salve o arquivo e atualize a página no celular.
"""
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNTIME = ROOT / "mixins" / "pocketpad" / "runtime.rs"
PAGE = ROOT / "pagina" / "pocketpad.html"
START = 'pub const PAGE: &str = r####"'
END = '"####;'


def game_home() -> Path:
    for cand in (
        os.environ.get("HORIZON_HOME"),
        os.environ.get("APPDATA") and str(Path(os.environ["APPDATA"]) / "horizon"),
        str(Path.home() / ".local" / "share" / "horizon"),
    ):
        if cand and (Path(cand) / "options.txt").is_file():
            return Path(cand)
    sys.exit("pasta do jogo não encontrada (defina HORIZON_HOME)")


def split(src: str):
    a = src.index(START) + len(START)
    b = src.index(END, a)
    return src[:a], src[a:b], src[b:]


def main() -> None:
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "extrair":
        _, page, _ = split(RUNTIME.read_text(encoding="utf-8"))
        PAGE.parent.mkdir(exist_ok=True)
        PAGE.write_text(page, encoding="utf-8", newline="\n")
        print(f"{PAGE} ({len(page)} bytes)")
    elif cmd == "embutir":
        page = PAGE.read_text(encoding="utf-8")
        if '"####' in page:
            sys.exit('a página não pode conter "####')
        head, _, tail = split(RUNTIME.read_text(encoding="utf-8"))
        RUNTIME.write_text(head + page + tail, encoding="utf-8", newline="\n")
        print(f"embutida em {RUNTIME}")
    elif cmd == "instalar":
        dest = game_home() / "config" / "pocketpad.html"
        dest.parent.mkdir(exist_ok=True)
        dest.write_bytes(PAGE.read_bytes())
        print(f"{dest}: atualize a página no celular")
    elif cmd == "remover":
        dest = game_home() / "config" / "pocketpad.html"
        dest.unlink(missing_ok=True)
        print("removida: vale a página embutida no mod")
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
