# PocketPad

**O celular vira o controle do [Horizon](https://github.com/horizon-rust/horizon)**, e você joga olhando para a tela
do PC. Native mod (horizon-mixins) com tema Minecraft: D-pad e botões no estilo do Minecraft de celular, inventário,
criativo, bancada e chat na tela do celular, vibração de controle e o seu personagem com a sua skin.

```
celular (página do controle, tema Minecraft)
   │ WebSocket na rede local (input a até 60/s, ping a cada 1 s)
   ▼
mod dentro do jogo (servidor HTTP + WebSocket na porta 8790)
   │ a cada frame: o controle virtual do jogo (`Game::pad_command`)
   ▼
Horizon trata como um gamepad: andar, olhar, minerar, inventário, menus
```

## Instalar

> **Native mods não têm sandbox:** o código roda com acesso total ao seu computador. Instale só se confiar nele
> (o código está todo aqui, em `mixins/pocketpad/`).

**Pelo launcher:** página **Mods** → **Horizon** → PocketPad → instalar → **Build**. O launcher baixa o Rust dele e
compila o jogo com o mod (a primeira vez leva uns 10 minutos).

**Pela linha de comando**, com o `.hnmod` da página de releases ou com esta pasta:

```powershell
horizon-launcher native-build <pasta mixins\pocketpad ou .hnmod descompactado> --instance <instância> --trust-native-code
```

Depois:

1. Abra o jogo. Na primeira vez, o Windows pergunta sobre o firewall: libere em **redes privadas**.
2. No canto de cima à direita aparece uma janela no estilo do jogo com o QR. Leia com o celular **no mesmo Wi-Fi** e
   toque em **Jogar**.
3. Com o celular ligado a janela some e fica só o ping (verde, amarelo ou vermelho). `/pocketpad` (ou `/celular`)
   mostra o endereço e mostra ou esconde o QR. O mod liga e desliga em Opções > Mods.

O controle precisa estar ligado em Opções > Controles (já vem ligado). Um celular por vez: o mais novo assume.

## Botões (como o Minecraft de celular)

| Celular | Jogo | Menus |
|---|---|---|
| D-pad à esquerda | Andar; com ↑ seguro aparecem as diagonais; dois toques em ↑ correm | Mover o foco |
| Meio do D-pad | Agachar (alterna) | OK |
| Arrastar na tela (bolinha) | Olhar (sensibilidade na engrenagem) | |
| Toque rápido na tela | Usar / colocar; ataca se a mira estiver num mob | |
| Segurar parado na tela | Quebrar / minerar (dá para mirar arrastando) | |
| Botão **Usar** | Segurar: comer, arco, escudo | |
| Botão grande à direita | Pular / nadar / voar | Voltar |
| Hotbar | Toque seleciona; segurar solta o item | |
| `…` ao lado da hotbar | Inventário | |
| Topo: pausa, chat, câmera, ajustes | | |

## O que aparece no celular

- **Hotbar e vida** (os corações somem no criativo), com os itens desenhados pelos modelos do próprio jogo: blocos
  em 3D, camas, baús, estandartes, cabeças, poções com a cor do efeito.
- **Inventário, bancada e criativo** desenhados no celular, sem o jogo de fundo, com as texturas do jogo e o seu
  personagem (a skin da sua conta). Toque pega e coloca, segurar faz o clique direito, "Mover rápido" é o shift.
  O catálogo do criativo fica em cache no celular: trocar de aba, rolar e buscar não esperam o PC.
- **Chat**: mensagens ao vivo e o teclado do celular para escrever (comandos com `/` também).
- **Vibração**: o mod ouve a vibração de controle do jogo (dano, bloco quebrado, explosão, totem) e o celular vibra
  junto. No iPhone o Safari não vibra.
- Os textos usam a fonte do próprio jogo.

## Por que WebSocket e não WebRTC

Um native mod só pode usar as crates que já estão no `Cargo.lock` do Horizon, e lá não tem WebRTC. Na mesma rede
Wi-Fi, um WebSocket com `TCP_NODELAY` mede ~2 ms de ida e volta, e o jogo solta o controle se o celular some por
700 ms. Jogar de fora de casa (NAT/TURN) fica para depois.

## Compatibilidade

Fixado no Horizon `ba6344a` (Nightly 2026-10-30), Minecraft 26.3. Além dos hook points estáveis (HUD, comando, lista
de mods), o mod se encaixa em `poll_control` e `pad_rumble` e lê partes internas do jogo (controle virtual, janelas,
criativo, skin). Por isso cada Nightly nova pede uma versão do mod testada nela.

## Desenvolver

```sh
cargo test                       # protocolo, WebSocket, HTTP, QR, ícones, skin
python tools/pagina.py instalar  # pagina/pocketpad.html -> pasta do jogo: vale na hora, sem recompilar
python tools/pagina.py embutir   # põe a página no runtime.rs para o próximo build
python tools/pagina.py remover   # volta a valer a página embutida
python tools/empacotar.py        # dist/pocketpad-<versão>.hnmod, o pacote do registro
```

`runtime.rs` é ao mesmo tempo a biblioteca dos testes e o módulo injetado no jogo, e só usa `std`. Os hooks
(`tick.rs`, `rumble.rs`, `hud.rs`, `command.rs`, `list.rs`) são pequenos. A página do controle é `pagina/pocketpad.html`;
`http://<ip>:8790/?k=x&demo` mostra ela sem conexão (`&inv`, `&inv&bancada`, `&cr`, `&chat`, `&fwd`: as outras telas).

Para mudar o Rust é preciso recompilar o jogo (`native-build`): o Horizon não carrega plugins em tempo de execução.

## Licença

MIT. Inspirado no 4Boxed. Não é feito nem endossado pelo projeto Horizon nem pela Mojang.
