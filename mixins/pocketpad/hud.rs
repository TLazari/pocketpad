// O canto de cima à direita: antes de um celular ligar, uma janela no estilo das
// do jogo (cinza com borda chanfrada) com o QR num quadro afundado, como um slot;
// com o celular ligado, só o ping.
fn pocketpad_hud(
    game: &mut crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    corners: &mut horizon_client::extras_hud::Corners,
) {
    use crate::game::pocketpad as m;
    use horizon_client::extras::Corner;
    if !game.native_on(m::ID) || !game.native_corner_shown(Corner::TopRight) {
        return;
    }
    // As cores das janelas do jogo (inventory.png).
    const BODY: u32 = 0xFFC6_C6C6;
    const EDGE: u32 = 0xFF00_0000;
    const LIGHT: u32 = 0xFFFF_FFFF;
    const SHADE: u32 = 0xFF55_5555;
    const SLOT_DARK: u32 = 0xFF37_3737;
    const TITLE: u32 = 0x40_4040;
    const SUB: u32 = 0x55_5555;
    let s = m::status();

    // Conectado: só o ping, verde, amarelo ou vermelho.
    if s.connected {
        let (line, color) = match s.ping_ms {
            Some(ms) if ms < 60 => (format!("{ms} ms"), 0x55_FF55),
            Some(ms) if ms < 150 => (format!("{ms} ms"), 0xFF_FF55),
            Some(ms) => (format!("{ms} ms"), 0xFF_5555),
            None => ("... ms".to_owned(), 0xAA_AAAA),
        };
        let w = game.native_text_width(&line) + 2.0;
        let (x, y) = corners.place(Corner::TopRight, w, 10.0);
        game.native_text(batch, &line, [x + 1.0, y + 1.0], color, true);
        return;
    }
    if !m::qr_shown() {
        return;
    }

    // Os textos no idioma do jogo.
    let lang = game.settings.language.clone();
    let (scan, starting, error) = match lang.get(..2) {
        Some("pt") => ("Leia o QR no mesmo Wi-Fi", "Ligando...", "Erro"),
        Some("es") => ("Escanea el QR en la misma Wi-Fi", "Iniciando...", "Error"),
        _ => ("Scan the QR on the same Wi-Fi", "Starting...", "Error"),
    };
    let short = s.url.trim_start_matches("http://").to_owned();
    let (lines, grid): (Vec<String>, _) = match &s.error {
        Some(e) => (vec![format!("{error}: {e}")], None),
        None if s.url.is_empty() => (vec![starting.to_owned()], None),
        None => (vec![scan.to_owned(), short], m::qr_cached(&s.url)),
    };

    // Medidas em pixels da interface, como as janelas do jogo (título em 8, 6).
    let cell = 2.0f32;
    let qr_w = grid.as_ref().map_or(0.0, |g| (g.len() + 4) as f32 * cell);
    let title_w = game.native_text_width(m::NAME);
    let text_w = lines
        .iter()
        .map(|l| game.native_text_width(l))
        .fold(title_w, f32::max);
    let w = (text_w.max(qr_w + 2.0) + 16.0).round();
    let text_h = 10.0 * lines.len() as f32;
    let h = (6.0 + 11.0 + text_h + if grid.is_some() { qr_w + 2.0 + 6.0 } else { 0.0 } + 5.0).round();
    let (x, y) = corners.place(Corner::TopRight, w, h);

    // A janela: contorno preto com os cantos cortados, luz em cima e à esquerda,
    // sombra embaixo e à direita.
    game.native_fill(batch, [x + 1.0, y + 1.0, w - 2.0, h - 2.0], BODY);
    game.native_fill(batch, [x + 2.0, y, w - 4.0, 1.0], EDGE);
    game.native_fill(batch, [x + 2.0, y + h - 1.0, w - 4.0, 1.0], EDGE);
    game.native_fill(batch, [x, y + 2.0, 1.0, h - 4.0], EDGE);
    game.native_fill(batch, [x + w - 1.0, y + 2.0, 1.0, h - 4.0], EDGE);
    for (cx, cy) in [(1.0, 1.0), (w - 2.0, 1.0), (1.0, h - 2.0), (w - 2.0, h - 2.0)] {
        game.native_fill(batch, [x + cx, y + cy, 1.0, 1.0], EDGE);
    }
    game.native_fill(batch, [x + 2.0, y + 1.0, w - 5.0, 2.0], LIGHT);
    game.native_fill(batch, [x + 1.0, y + 2.0, 2.0, h - 5.0], LIGHT);
    game.native_fill(batch, [x + 3.0, y + h - 3.0, w - 5.0, 2.0], SHADE);
    game.native_fill(batch, [x + w - 3.0, y + 3.0, 2.0, h - 5.0], SHADE);
    game.native_fill(batch, [x + w - 3.0, y + 2.0, 1.0, 1.0], BODY);
    game.native_fill(batch, [x + 2.0, y + h - 3.0, 1.0, 1.0], BODY);

    // Título e textos, escuros e sem sombra, como nos inventários.
    game.native_text(batch, m::NAME, [x + 8.0, y + 6.0], TITLE, false);
    for (i, line) in lines.iter().enumerate() {
        game.native_text(batch, line, [x + 8.0, y + 17.0 + 10.0 * i as f32], SUB, false);
    }

    // O QR num quadro afundado (escuro em cima e à esquerda, claro embaixo).
    if let Some(grid) = grid {
        let qx = (x + (w - qr_w) / 2.0).round();
        let qy = y + 17.0 + text_h + 4.0;
        game.native_fill(batch, [qx - 1.0, qy - 1.0, qr_w + 1.0, 1.0], SLOT_DARK);
        game.native_fill(batch, [qx - 1.0, qy - 1.0, 1.0, qr_w + 1.0], SLOT_DARK);
        game.native_fill(batch, [qx, qy + qr_w, qr_w + 1.0, 1.0], LIGHT);
        game.native_fill(batch, [qx + qr_w, qy, 1.0, qr_w + 1.0], LIGHT);
        game.native_fill(batch, [qx, qy, qr_w, qr_w], 0xFFFF_FFFF);
        for (cx, cy, run) in m::qr_runs(&grid) {
            let rect = [
                qx + (cx + 2) as f32 * cell,
                qy + (cy + 2) as f32 * cell,
                run as f32 * cell,
                cell,
            ];
            game.native_fill(batch, rect, 0xFF00_0000);
        }
    }
}
