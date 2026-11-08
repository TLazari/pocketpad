// PocketPad: o celular vira o controle do Horizon, olhando para a tela
// do PC (a ideia do /c do 4Boxed). O mod abre na rede local um servidor HTTP
// com a página do controle e um WebSocket por onde chega o input; a cada
// iteração o jogo aplica esse input no controle virtual (`pad_command`, o mesmo
// de `horizon-ctl pad`), então tudo que um gamepad faz funciona: andar, olhar,
// minerar e menus. O celular também mostra a hotbar, a vida e a janela aberta
// no PC (inventário, bancada, fornalha, baú...) para mexer nos itens pelo
// toque, o chat com o teclado do celular, e vibra quando o jogo vibraria o
// controle.
//
// Este arquivo é a biblioteca Cargo do mod (os testes) e o módulo que o bundle
// injeta no jogo (`crate::game::pocketpad`). Só usa `std`: o jogo não
// recebe dependências novas. Sem `//!`, `mod` externo nem `include!` (regras do
// horizon-mixins).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const ID: &str = "pocketpad";
pub const NAME: &str = "PocketPad";
pub const VERSION: &str = "1.0.0";
pub const DESCRIPTION: &str = "Play with your phone as the controller. Open this page and pick \"Connect phone\" to show the QR code; /pocketpad shows or hides it too.";
/// Primeira porta tentada; se estiver ocupada, as próximas 9.
pub const PORT: u16 = 8790;
/// Sem mensagem do celular por esse tempo, o controle é solto (celular caiu).
pub const STALE: Duration = Duration::from_millis(700);

/// Nomes dos botões do controle virtual, na ordem dos bits que o celular manda.
pub const BUTTONS: [&str; 16] = [
    "south", "east", "west", "north", "lb", "rb", "lt", "rt", "select", "start", "ls", "rs", "up",
    "down", "left", "right",
];

/// O estado do controle que o celular mandou por último.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    pub lt: f32,
    pub rt: f32,
    pub buttons: u32,
}

/// `i lx ly rx ry lt rt botões`: sticks -1..1 (para cima positivo), gatilhos 0..1.
pub fn parse_input(line: &str) -> Option<Input> {
    let mut it = line.split_ascii_whitespace();
    if it.next()? != "i" {
        return None;
    }
    let mut n = || -> Option<f32> {
        let v: f32 = it.next()?.parse().ok()?;
        v.is_finite().then_some(v)
    };
    let (lx, ly, rx, ry, lt, rt) = (n()?, n()?, n()?, n()?, n()?, n()?);
    let buttons: u32 = it.next()?.parse().ok()?;
    let s = |v: f32| v.clamp(-1.0, 1.0);
    let t = |v: f32| v.clamp(0.0, 1.0);
    Some(Input {
        lx: s(lx),
        ly: s(ly),
        rx: s(rx),
        ry: s(ry),
        lt: t(lt),
        rt: t(rt),
        buttons: buttons & 0xFFFF,
    })
}

/// As linhas de `pad` que levam o controle virtual de `from` a `to`.
pub fn diff(from: &Input, to: &Input) -> Vec<String> {
    let mut out = Vec::new();
    let moved = |a: f32, b: f32| (a - b).abs() > 0.004;
    if moved(from.lx, to.lx) || moved(from.ly, to.ly) {
        out.push(format!("pad stick left {:.3} {:.3}", to.lx, to.ly));
    }
    if moved(from.rx, to.rx) || moved(from.ry, to.ry) {
        out.push(format!("pad stick right {:.3} {:.3}", to.rx, to.ry));
    }
    if moved(from.lt, to.lt) {
        out.push(format!("pad trigger left {:.3}", to.lt));
    }
    if moved(from.rt, to.rt) {
        out.push(format!("pad trigger right {:.3}", to.rt));
    }
    for (i, name) in BUTTONS.iter().enumerate() {
        let bit = 1u32 << i;
        if from.buttons & bit != to.buttons & bit {
            let state = if to.buttons & bit != 0 { "down" } else { "up" };
            out.push(format!("pad {name} {state}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Estado compartilhado entre o servidor (threads de rede) e o jogo.

/// O que vai para o celular: (opcode do WebSocket, conteúdo).
type Outgoing = (u8, Vec<u8>);

/// O que o celular pede além do controle: o jogo executa no próximo frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Clique num slot da janela aberta (ids de `Game::slots`).
    Click { slot: u32, right: bool, shift: bool },
    /// Seleciona o slot 0..8 da hotbar.
    Select(u8),
    /// Abre o inventário (fora de menus).
    OpenInventory,
    /// Fecha o inventário ou a estação aberta.
    Close,
    /// Manda uma mensagem ou um comando (`/...`) no chat.
    Chat(String),
    /// Aba anterior (`false`) ou próxima (`true`): criativo, livro de receitas.
    Tab(bool),
    /// Uma aba do inventário criativo, pelo índice.
    CreativeTab(u8),
    /// Rola a lista do criativo: positivo sobe, negativo desce (linhas).
    Scroll(i8),
    /// A lixeira do criativo: apaga o item na mão.
    Trash,
    /// Um item do catálogo do criativo (aba, posição na lista da aba), clicado
    /// como no slot de exibição do jogo: direito e shift como no PC.
    Pick { tab: u8, index: u32, right: bool, shift: bool },
    /// A busca do criativo feita pelo jogo (`Creative::search_results`): o
    /// celular manda o texto com um número e recebe as posições na aba de busca.
    Search { seq: u32, text: String },
    /// A dica de um item (`stack_tooltip`, como o jogo mostra ao passar o mouse):
    /// os encantamentos de um livro, os efeitos de uma poção...
    Tooltip { seq: u32, of: TipOf },
}

/// De qual item é a dica.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TipOf {
    /// Um item do catálogo do criativo (aba, posição).
    Catalog(u8, u32),
    /// Um slot da janela aberta.
    Slot(u32),
    /// O item na mão (no cursor).
    Carried,
}

/// Lê uma ação do celular: `k slot direito shift`, `h n`, `e`, `q`, `t texto`, `b 0|1`.
pub fn parse_action(line: &str) -> Option<Action> {
    let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
    match head {
        "k" => {
            let mut it = rest.split_ascii_whitespace();
            let slot: u32 = it.next()?.parse().ok()?;
            let right = it.next() == Some("1");
            let shift = it.next() == Some("1");
            (slot < 10_000).then_some(Action::Click { slot, right, shift })
        }
        "h" => {
            let n: u8 = rest.trim().parse().ok()?;
            (n < 9).then_some(Action::Select(n))
        }
        "e" => Some(Action::OpenInventory),
        "q" => Some(Action::Close),
        "t" => {
            let text: String = rest.trim().chars().filter(|c| !c.is_control()).take(256).collect();
            (!text.is_empty()).then_some(Action::Chat(text))
        }
        "b" => Some(Action::Tab(rest.trim() == "1")),
        "ct" => {
            let n: u8 = rest.trim().parse().ok()?;
            (n < 32).then_some(Action::CreativeTab(n))
        }
        "w" => {
            let n: i8 = rest.trim().parse().ok()?;
            (n != 0 && (-9..=9).contains(&n)).then_some(Action::Scroll(n))
        }
        "d" => Some(Action::Trash),
        "gi" => {
            let mut it = rest.split_ascii_whitespace();
            let tab: u8 = it.next()?.parse().ok()?;
            let index: u32 = it.next()?.parse().ok()?;
            let right = it.next() == Some("1");
            let shift = it.next() == Some("1");
            (tab < 32 && index < 100_000).then_some(Action::Pick { tab, index, right, shift })
        }
        "sq" => {
            // O texto vai cru (espaços contam, como no jogo), até 50 caracteres.
            let (seq, text) = rest.split_once(' ').unwrap_or((rest, ""));
            let seq: u32 = seq.parse().ok()?;
            let text: String = text.chars().filter(|c| !c.is_control()).take(50).collect();
            Some(Action::Search { seq, text })
        }
        "tt" => {
            let mut it = rest.split_ascii_whitespace();
            let seq: u32 = it.next()?.parse().ok()?;
            let of = match it.next()? {
                "c" => {
                    let tab: u8 = it.next()?.parse().ok()?;
                    let index: u32 = it.next()?.parse().ok()?;
                    (tab < 32 && index < 100_000).then_some(TipOf::Catalog(tab, index))?
                }
                "s" => {
                    let slot: u32 = it.next()?.parse().ok()?;
                    (slot < 10_000).then_some(TipOf::Slot(slot))?
                }
                "m" => TipOf::Carried,
                _ => return None,
            };
            Some(Action::Tooltip { seq, of })
        }
        _ => None,
    }
}

#[derive(Default)]
struct Shared {
    url: String,
    key: String,
    error: Option<String>,
    /// Conta as conexões: só a mais nova controla.
    generation: u64,
    connected: bool,
    input: Input,
    last: Option<Instant>,
    connected_at: Option<Instant>,
    ping_ms: Option<u32>,
    tx: Option<mpsc::Sender<Outgoing>>,
    actions: VecDeque<Action>,
}

fn shared() -> MutexGuard<'static, Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    let m = SHARED.get_or_init(|| Mutex::new(Shared::default()));
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Manda uma linha de texto para o celular ligado, se houver.
fn send_text(line: &str) {
    if let Some(tx) = &shared().tx {
        let _ = tx.send((0x1, line.as_bytes().to_vec()));
    }
}

/// O que o HUD mostra.
pub struct Status {
    pub url: String,
    pub error: Option<String>,
    pub connected: bool,
    pub connected_for: Option<Duration>,
    pub ping_ms: Option<u32>,
}

pub fn status() -> Status {
    let s = shared();
    Status {
        url: s.url.clone(),
        error: s.error.clone(),
        connected: s.connected,
        connected_for: s.connected_at.map(|t| t.elapsed()),
        ping_ms: s.ping_ms,
    }
}

// ---------------------------------------------------------------------------
// O lado do jogo: chamado uma vez por iteração do loop.

#[derive(Default)]
struct Applied {
    input: Input,
    generation: u64,
    /// `/pocketpad` mostrou (true) ou escondeu (false) o QR do canto.
    qr_shown: Option<bool>,
    /// "QR no canto ao entrar" (Opções > Mods), lido uma vez do arquivo do mod.
    qr_auto: Option<bool>,
    /// O QR grande no meio da tela ("Conectar celular"), até um celular ligar.
    qr_big: bool,
    /// O último texto mandado de cada tipo (`s` estado, `c` chat): só muda manda.
    sent: HashMap<char, String>,
    last_snapshot: Option<Instant>,
    last_chat: Option<Instant>,
}

thread_local! {
    static APPLIED: RefCell<Applied> = RefCell::new(Applied::default());
}

/// O que o jogo faz nesta iteração.
#[derive(Debug, Default)]
pub struct Tick {
    /// Linhas `pad ...` para `Game::pad_command`.
    pub pad: Vec<String>,
    /// O que o celular pediu.
    pub actions: Vec<Action>,
    /// Hora de mandar o estado (hotbar, vida, janela aberta).
    pub snapshot: bool,
    /// Hora de mandar o chat.
    pub chat: bool,
}

/// Uma vez por iteração: liga o servidor e diz o que aplicar. `on` é o mod
/// ligado em Opções > Mods.
pub fn pump(on: bool) -> Tick {
    APPLIED.with(|cell| {
        let mut a = cell.borrow_mut();
        let mut tick = Tick::default();
        if !on {
            if a.input != Input::default() {
                tick.pad.push("pad release".to_owned());
            }
            a.input = Input::default();
            return tick;
        }
        start();
        let (target, generation, connected, actions) = {
            let mut s = shared();
            let fresh = s.connected && s.last.is_some_and(|t| t.elapsed() < STALE);
            let target = if fresh { s.input } else { Input::default() };
            let actions: Vec<Action> = s.actions.drain(..).collect();
            (target, s.generation, s.connected, actions)
        };
        if generation != a.generation {
            // Outro celular assumiu: começa do controle solto e manda tudo de novo.
            if a.input != Input::default() {
                tick.pad.push("pad release".to_owned());
            }
            a.input = Input::default();
            a.generation = generation;
            a.sent.clear();
            a.last_snapshot = None;
            a.last_chat = None;
        }
        tick.pad.extend(diff(&a.input, &target));
        a.input = target;
        if connected {
            let due = |t: Option<Instant>, every: u64| {
                t.is_none_or(|t| t.elapsed() >= Duration::from_millis(every))
            };
            tick.snapshot = !actions.is_empty() || due(a.last_snapshot, 100);
            tick.chat = actions.iter().any(|x| matches!(x, Action::Chat(_)))
                || due(a.last_chat, 500);
            if tick.snapshot {
                a.last_snapshot = Some(Instant::now());
            }
            if tick.chat {
                a.last_chat = Some(Instant::now());
            }
            tick.actions = actions;
        }
        tick
    })
}

/// Manda `kind body` ao celular se mudou desde o último envio desse tipo.
pub fn publish(kind: char, body: &str) {
    let changed = APPLIED.with(|cell| {
        let mut a = cell.borrow_mut();
        if a.sent.get(&kind).is_some_and(|b| b == body) {
            return false;
        }
        a.sent.insert(kind, body.to_owned());
        true
    });
    if changed {
        send_text(&format!("{kind} {body}"));
    }
}

/// A vibração que o jogo mandou ao controle (`Rumble::pulse`): o celular
/// vibra igual, com a força simulada pelo ritmo.
pub fn rumble(strong: f32, weak: f32, millis: u32) {
    if shared().connected {
        send_text(&format!("r {strong:.2} {weak:.2} {}", millis.min(2000)));
    }
}

/// O QR sem celular ligado: o grande ("Conectar celular" em Opções > Mods) ou
/// o do canto (`/pocketpad`, ou "QR no canto ao entrar" ligado).
pub fn qr_shown() -> bool {
    APPLIED.with(|cell| {
        let a = cell.borrow();
        a.qr_big || a.qr_shown.unwrap_or(a.qr_auto.unwrap_or(false))
    })
}

/// O QR grande, no meio da tela.
pub fn qr_big() -> bool {
    APPLIED.with(|cell| cell.borrow().qr_big)
}

pub fn show_qr_big() {
    APPLIED.with(|cell| cell.borrow_mut().qr_big = true);
}

/// Um celular ligou: o QR grande já fez o trabalho dele.
pub fn qr_connected() {
    APPLIED.with(|cell| cell.borrow_mut().qr_big = false);
}

/// `/pocketpad`: mostra ou esconde o QR do canto (e fecha o grande).
pub fn toggle_qr() -> bool {
    APPLIED.with(|cell| {
        let mut a = cell.borrow_mut();
        let shown = !(a.qr_big || a.qr_shown.unwrap_or(a.qr_auto.unwrap_or(false)));
        a.qr_big = false;
        a.qr_shown = Some(shown);
        shown
    })
}

/// "QR no canto ao entrar": `load` lê o arquivo do mod na primeira vez.
pub fn qr_auto(load: impl FnOnce() -> Option<String>) -> bool {
    if let Some(v) = APPLIED.with(|cell| cell.borrow().qr_auto) {
        return v;
    }
    let v = load().as_deref() == Some("1");
    APPLIED.with(|cell| cell.borrow_mut().qr_auto = Some(v));
    v
}

pub fn set_qr_auto(on: bool) {
    APPLIED.with(|cell| {
        let mut a = cell.borrow_mut();
        a.qr_auto = Some(on);
        a.qr_shown = None;
    });
}

/// O código da sala: a chave que o celular manda (`?k=` no QR, ou digitada).
pub fn room_code() -> String {
    shared().key.clone()
}

/// O endereço sem a chave (`192.168.1.172:8790`), para digitar no celular.
pub fn short_address(url: &str) -> String {
    let u = url.trim_start_matches("http://");
    u.split(['/', '?']).next().unwrap_or(u).to_owned()
}

/// Os textos do mod no jogo, no idioma dele (inglês nos que o mod não traduz).
pub fn text(lang: &str, key: &str) -> &'static str {
    let pt = lang.starts_with("pt");
    let es = lang.starts_with("es");
    match key {
        "scan" if pt => "Leia o QR no mesmo Wi-Fi",
        "scan" if es => "Escanea el QR en la misma Wi-Fi",
        "scan" => "Scan the QR on the same Wi-Fi",
        "code" if pt => "Código",
        "code" if es => "Código",
        "code" => "Code",
        "starting" if pt => "Ligando...",
        "starting" if es => "Iniciando...",
        "starting" => "Starting...",
        "error" => "Error",
        "connect" if pt => "Conectar celular (mostrar QR)",
        "connect" if es => "Conectar el móvil (mostrar QR)",
        "connect" => "Connect phone (show QR)",
        "connected" if pt => "Celular conectado",
        "connected" if es => "Móvil conectado",
        "connected" => "Phone connected",
        "auto" if pt => "QR no canto ao entrar",
        "auto" if es => "QR en la esquina al entrar",
        "auto" => "QR in the corner on join",
        "on" if pt => "Sim",
        "on" if es => "Sí",
        "on" => "On",
        "off" if pt => "Não",
        "off" if es => "No",
        "off" => "Off",
        "address" if pt => "Endereço",
        "address" if es => "Dirección",
        "address" => "Address",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Catálogo do criativo: o celular guarda em cache e navega sozinho.

#[derive(Default)]
struct Catalog {
    json: String,
    /// O idioma em que os títulos das abas foram escritos.
    lang: String,
    version: String,
    ids: Vec<String>,
}

fn catalog() -> MutexGuard<'static, Catalog> {
    static CATALOG: OnceLock<Mutex<Catalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| Mutex::new(Catalog::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Ainda não há catálogo: o jogo monta um (e de novo se o idioma mudar).
/// Ainda sem catálogo, ou feito noutro idioma (os títulos das abas mudam).
pub fn catalog_needed(lang: &str) -> bool {
    let c = catalog();
    c.json.is_empty() || c.lang != lang
}

/// FNV-1a de 64 bits, em hexadecimal: a versão do catálogo.
pub fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Guarda o catálogo que o jogo montou (`json`) e os ids dos itens nele.
pub fn set_catalog(json: String, ids: Vec<String>, lang: &str) {
    {
        let mut c = catalog();
        c.lang = lang.to_owned();
        c.version = fnv64(json.as_bytes());
        c.json = json;
        c.ids = ids;
    }
    // Monta o mapa de ícones em segundo plano (lê milhares de arquivos): quando
    // o celular pedir, já está pronto.
    let _ = std::thread::Builder::new()
        .name("pocketpad-icones".into())
        .spawn(|| {
            let _ = icons_json();
            let _ = bundle_json();
        });
}

/// Todos os `items/*.json` e `models/*.json` do jogo num JSON só
/// (`{"items/x.json": {...}, "models/block/y.json": {...}}`, ~1,5 MB): o celular
/// baixa uma vez na tela de carregamento e monta os itens sem outro pedido.
/// Montado uma vez por sessão.
pub fn bundle_json() -> &'static str {
    static BUNDLE: OnceLock<String> = OnceLock::new();
    BUNDLE.get_or_init(|| match assets_dir() {
        Some(dir) => build_bundle(dir),
        None => "{}".to_owned(),
    })
}

pub fn build_bundle(assets: &Path) -> String {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else if path.extension().is_some_and(|x| x == "json") {
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    files.push((rel, path));
                }
            }
        }
    }
    let mut files = Vec::new();
    for sub in ["items", "models"] {
        walk(assets, &assets.join(sub), &mut files);
    }
    files.sort();
    let mut out = String::with_capacity(1_600_000);
    out.push('{');
    let mut first = true;
    for (rel, path) in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim_start_matches('\u{feff}').trim();
        // Só objetos JSON, e chaves sem nada a escapar.
        if !text.starts_with('{') || !text.ends_with('}') || !safe_path(&rel) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(&rel);
        out.push_str("\":");
        out.push_str(text);
    }
    out.push('}');
    out
}

/// A versão do catálogo, para o celular saber se o cache dele vale.
pub fn catalog_version() -> String {
    catalog().version.clone()
}

/// `{"v": versão, "icons": {id: ícone}}` de todos os itens do catálogo.
fn icons_json() -> String {
    static CACHE: OnceLock<Mutex<(String, String)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((String::new(), String::new())));
    let (version, ids) = {
        let c = catalog();
        (c.version.clone(), c.ids.clone())
    };
    if let Some(hit) = {
        let g = cache.lock().unwrap_or_else(|e| e.into_inner());
        (g.0 == version && !g.1.is_empty()).then(|| g.1.clone())
    } {
        return hit;
    }
    let mut out = format!(r#"{{"v":"{version}","icons":{{"#);
    let mut seen = std::collections::HashSet::new();
    for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
        if !out.ends_with('{') {
            out.push(',');
        }
        out.push_str(&format!(r#""{id}":{}"#, cached_icon(id)));
    }
    out.push_str("}}");
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = (version, out.clone());
    out
}

// ---------------------------------------------------------------------------
// Texturas e ícones: lidos dos assets que o launcher extraiu do Minecraft.

/// `assets/minecraft` do jogo: o `--resources` da linha de comando, ou o
/// `.local/horizon-client/<versão>` ao lado do executável (instalação do launcher).
pub fn assets_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let args: Vec<String> = std::env::args().collect();
        let mut roots: Vec<PathBuf> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if a == "--resources" {
                if let Some(p) = args.get(i + 1) {
                    roots.push(p.into());
                }
            } else if let Some(p) = a.strip_prefix("--resources=") {
                roots.push(p.into());
            }
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            if let Ok(rd) = fs::read_dir(dir.join(".local").join("horizon-client")) {
                roots.extend(rd.flatten().map(|e| e.path()));
            }
        }
        roots
            .into_iter()
            .map(|r| r.join("assets").join("minecraft"))
            .find(|p| p.join("textures").is_dir())
    })
    .as_deref()
}

/// A pasta do jogo (a de `options.txt`): `$HORIZON_HOME`, a pasta atual, ou a
/// padrão do sistema (`%APPDATA%\horizon`, `~/.local/share/horizon`).
pub fn game_home() -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(h) = std::env::var_os("HORIZON_HOME") {
        found.push(h.into());
    }
    if let Ok(cwd) = std::env::current_dir() {
        found.push(cwd);
    }
    if let Some(a) = std::env::var_os("APPDATA") {
        found.push(PathBuf::from(a).join("horizon"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        found.push(PathBuf::from(h).join(".local").join("share").join("horizon"));
    }
    found.into_iter().find(|p| p.join("options.txt").is_file())
}

/// `config/pocketpad.html` na pasta do jogo, se existir.
pub fn page_file() -> Option<PathBuf> {
    let p = game_home()?.join("config").join("pocketpad.html");
    p.is_file().then_some(p)
}

/// Um caminho seguro dentro dos assets: só `[a-z0-9_./-]`, sem `..`.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.contains("..")
        && !p.starts_with('/')
        && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b))
}

/// O valor string de `"key"` (o primeiro que for string).
pub fn json_str(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    text.match_indices(&needle).find_map(|(i, _)| {
        let rest = text[i + needle.len()..].trim_start();
        let rest = rest.strip_prefix(':')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        rest.find('"').map(|end| rest[..end].to_owned())
    })
}

/// Os pares do objeto `"textures": { ... }` de um modelo.
pub fn texture_map(text: &str) -> Vec<(String, String)> {
    let Some(i) = text.find("\"textures\"") else {
        return Vec::new();
    };
    let rest = &text[i..];
    let (Some(open), Some(close)) = (rest.find('{'), rest.find('}')) else {
        return Vec::new();
    };
    if close < open {
        return Vec::new();
    }
    let strings: Vec<&str> = rest[open + 1..close].split('"').skip(1).step_by(2).collect();
    strings
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| (c[0].to_owned(), c[1].to_owned()))
        .collect()
}

fn strip_ns(r: &str) -> &str {
    r.strip_prefix("minecraft:").unwrap_or(r)
}

/// O ícone de um item: `{"k":"flat","t":"item/stick"}`, `{"k":"cube","t":topo,
/// "s":lado,"f":frente}` (blocos, desenhados em isométrico como no inventário) ou
/// `{"k":"none"}`. Segue `items/<id>.json` e os `parent` dos modelos.
pub fn icon_json(assets: &Path, id: &str) -> String {
    const NONE: &str = r#"{"k":"none"}"#;
    let name = strip_ns(id);
    if !safe_path(name) || name.contains('/') {
        return NONE.into();
    }
    let read = |p: PathBuf| fs::read_to_string(p).ok();
    let first = read(assets.join("items").join(format!("{name}.json")))
        .and_then(|t| json_str(&t, "model").or_else(|| json_str(&t, "base")))
        .unwrap_or_else(|| {
            if assets.join("models").join("item").join(format!("{name}.json")).is_file() {
                format!("minecraft:item/{name}")
            } else {
                format!("minecraft:block/{name}")
            }
        });
    let mut textures: Vec<(String, String)> = Vec::new();
    let mut block = false;
    let mut next = Some(first);
    for _ in 0..8 {
        let Some(model) = next.take() else { break };
        let model = strip_ns(&model).to_owned();
        if !safe_path(&model) {
            break;
        }
        block |= model.starts_with("block/");
        let Some(text) = read(assets.join("models").join(format!("{model}.json"))) else {
            break;
        };
        for (k, v) in texture_map(&text) {
            if !textures.iter().any(|(kk, _)| *kk == k) {
                textures.push((k, v));
            }
        }
        next = json_str(&text, "parent");
    }
    let get = |key: &str| -> Option<String> {
        let mut v = textures.iter().find(|(k, _)| k == key)?.1.clone();
        for _ in 0..6 {
            match v.strip_prefix('#') {
                Some(r) => v = textures.iter().find(|(k, _)| k == r)?.1.clone(),
                None => break,
            }
        }
        let v = strip_ns(&v).to_owned();
        safe_path(&v).then_some(v)
    };
    let any = |keys: &[&str]| keys.iter().find_map(|k| get(k));
    if let Some(t) = get("layer0") {
        return format!(r#"{{"k":"flat","t":"{t}"}}"#);
    }
    if block {
        let top = any(&["top", "up", "end", "all", "texture"]);
        let side = any(&["side", "east", "west", "south", "all", "texture", "north", "front"]);
        if let Some(top) = top.clone().or_else(|| side.clone()) {
            let side = side.unwrap_or_else(|| top.clone());
            let front = any(&["front", "north"]).unwrap_or_else(|| side.clone());
            return format!(r#"{{"k":"cube","t":"{top}","s":"{side}","f":"{front}"}}"#);
        }
    }
    match get("particle") {
        Some(t) => format!(r#"{{"k":"flat","t":"{t}"}}"#),
        None => NONE.into(),
    }
}

fn cached_icon(id: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
        return v.clone();
    }
    let v = match assets_dir() {
        Some(dir) => icon_json(dir, id),
        None => r#"{"k":"none"}"#.into(),
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_owned(), v.clone());
    v
}

// ---------------------------------------------------------------------------
// Servidor: HTTP para a página e os assets, WebSocket para o controle.

/// Liga o servidor uma vez (as chamadas seguintes não fazem nada).
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let key = new_key();
        let ip = lan_ip();
        let listener = (PORT..PORT + 10).find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok());
        let mut s = shared();
        s.key = key.clone();
        match listener {
            Some(listener) => {
                let port = listener.local_addr().map(|a| a.port()).unwrap_or(PORT);
                s.url = format!("http://{ip}:{port}/?k={key}");
                drop(s);
                eprintln!("[{ID}] controle em {}", shared().url);
                let _ = std::thread::Builder::new()
                    .name("pocketpad".into())
                    .spawn(move || serve(listener));
                // O pacote de modelos lê ~5.800 arquivos (segundos com o disco
                // frio): monta já, para o celular não esperar na primeira conexão.
                let _ = std::thread::Builder::new()
                    .name("pocketpad-pacote".into())
                    .spawn(|| {
                        let _ = bundle_json();
                    });
            }
            None => {
                s.error = Some(format!("ports {PORT}-{} in use", PORT + 9));
                eprintln!("[{ID}] sem porta livre");
            }
        }
    });
}

fn serve(listener: TcpListener) {
    for stream in listener.incoming().flatten() {
        let _ = std::thread::Builder::new()
            .name("pocketpad-conexao".into())
            .spawn(move || {
                let _ = handle(stream);
            });
    }
}

/// Uma chave curta por sessão do jogo: só quem leu o QR controla.
fn new_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}-{}-{:?}", std::process::id(), Instant::now());
    let digest = sha1(seed.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// O IP desta máquina na rede local: o da interface da rota padrão. O `connect`
/// de UDP não manda nada, só escolhe a interface.
fn lan_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

struct Request {
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then_some(v)
        })
    }
}

pub fn parse_request(head: &str) -> Option<(String, String, Vec<(String, String)>)> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    if first.next()? != "GET" {
        return None;
    }
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Some((path.to_owned(), query.to_owned(), headers))
}

fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 16 * 1024 || stream.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    cache: bool,
    body: &[u8],
) -> io::Result<()> {
    let cache = if cache { "max-age=86400" } else { "no-store" };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn handle(mut stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let head = read_head(&mut stream)?;
    let Some((path, query, headers)) = parse_request(&head) else {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", false, b"");
    };
    let req = Request {
        path,
        query,
        headers,
    };
    let not_found = |s: &mut TcpStream| respond(s, "404 Not Found", "text/plain", false, b"");
    match req.path.as_str() {
        "/" | "/index.html" => {
            // Um `config/pocketpad.html` na pasta do jogo substitui a página
            // embutida: mudar o visual não precisa recompilar o jogo.
            let page = page_file().and_then(|p| fs::read(p).ok());
            let body = page.as_deref().unwrap_or(PAGE.as_bytes());
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", false, body)
        }
        "/icon" => {
            let id = req.param("id").unwrap_or("").replace("%3A", ":").replace("%3a", ":");
            let body = cached_icon(&id);
            respond(&mut stream, "200 OK", "application/json", true, body.as_bytes())
        }
        // A skin baixada da conta: o jogo guarda o PNG em
        // `<pasta do jogo>/assets/skins/<2 hex>/<sha1>`.
        p if p.starts_with("/tex/skins/") => {
            let location = p["/tex/".len()..].trim_end_matches(".png");
            match skin_file(location).and_then(|f| fs::read(f).ok()) {
                Some(bytes) => respond(&mut stream, "200 OK", "image/png", true, &bytes),
                None => not_found(&mut stream),
            }
        }
        p if p.starts_with("/tex/") => {
            let rel = &p[5..];
            match (assets_dir(), safe_path(rel) && rel.ends_with(".png")) {
                (Some(dir), true) => match fs::read(dir.join("textures").join(rel)) {
                    Ok(bytes) => respond(&mut stream, "200 OK", "image/png", true, &bytes),
                    Err(_) => not_found(&mut stream),
                },
                _ => not_found(&mut stream),
            }
        }
        // O catálogo do criativo e o mapa de ícones dele: o celular guarda os
        // dois e só volta a pedir quando a versão muda.
        "/creative.json" => {
            let json = catalog().json.clone();
            if json.is_empty() {
                not_found(&mut stream)
            } else {
                respond(&mut stream, "200 OK", "application/json", false, json.as_bytes())
            }
        }
        "/icons.json" => {
            let body = icons_json();
            respond(&mut stream, "200 OK", "application/json", false, body.as_bytes())
        }
        // Os modelos de entidade do vanilla (baú, shulker, estandarte, cabeças,
        // escudo, tridente, vaso, conduíte, golem de cobre, o jogador): os itens
        // que o jogo desenha por código saem com a mesma geometria.
        // Os modelos de todos os itens de uma vez (a tela de carregamento).
        "/bundle.json" => {
            let body = bundle_json();
            respond(&mut stream, "200 OK", "application/json", true, body.as_bytes())
        }
        "/entity-models.json" => {
            let file = assets_dir()
                .and_then(|d| d.parent()?.parent().map(|r| r.join("entity-models.json")));
            match file.and_then(|f| fs::read(f).ok()) {
                Some(bytes) => respond(&mut stream, "200 OK", "application/json", true, &bytes),
                None => not_found(&mut stream),
            }
        }
        // Os modelos crus (`items/`, `models/`): o celular monta a geometria
        // dos itens (cama, escada, cerca...) como o jogo desenha na interface.
        p if p.starts_with("/a/") => {
            let rel = &p[3..];
            let ok = safe_path(rel)
                && rel.ends_with(".json")
                && (rel.starts_with("items/") || rel.starts_with("models/"));
            match (assets_dir(), ok) {
                (Some(dir), true) => match fs::read(dir.join(rel)) {
                    Ok(bytes) => respond(&mut stream, "200 OK", "application/json", true, &bytes),
                    Err(_) => not_found(&mut stream),
                },
                _ => not_found(&mut stream),
            }
        }
        // A fonte do jogo: os bitmaps de `font/include/default.json`.
        "/font.json" => match assets_dir() {
            Some(dir) => match fs::read(dir.join("font").join("include").join("default.json")) {
                Ok(bytes) => respond(&mut stream, "200 OK", "application/json", true, &bytes),
                Err(_) => not_found(&mut stream),
            },
            None => not_found(&mut stream),
        },
        p if p.starts_with("/lang/") => {
            let code = p[6..].trim_end_matches(".json");
            let ok = !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
            match (assets_dir(), ok) {
                (Some(dir), true) => match fs::read(dir.join("lang").join(format!("{code}.json"))) {
                    Ok(bytes) => respond(&mut stream, "200 OK", "application/json", true, &bytes),
                    Err(_) => not_found(&mut stream),
                },
                _ => not_found(&mut stream),
            }
        }
        "/ws" => {
            let key_ok = req.param("k") == Some(shared().key.as_str());
            let upgrade = req
                .header("Upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
            match (key_ok, upgrade, req.header("Sec-WebSocket-Key")) {
                (true, true, Some(ws_key)) => {
                    let accept = ws_accept(ws_key);
                    let head = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    );
                    stream.write_all(head.as_bytes())?;
                    controller(stream)
                }
                (false, ..) => respond(&mut stream, "403 Forbidden", "text/plain", false, b"chave"),
                _ => respond(&mut stream, "400 Bad Request", "text/plain", false, b""),
            }
        }
        _ => not_found(&mut stream),
    }
}

/// Um celular ligado: lê o input até cair ou outro celular assumir.
fn controller(stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let (tx, rx) = mpsc::channel::<Outgoing>();
    let mut writer = stream.try_clone()?;
    std::thread::Builder::new()
        .name("celular-envio".into())
        .spawn(move || {
            for (opcode, payload) in rx {
                if write_frame(&mut writer, opcode, &payload).is_err() {
                    break;
                }
            }
            let _ = writer.shutdown(std::net::Shutdown::Both);
        })?;
    let generation = {
        let mut s = shared();
        if let Some(old) = s.tx.take() {
            let _ = old.send((0x1, b"x".to_vec()));
            let _ = old.send((0x8, Vec::new()));
        }
        s.generation += 1;
        s.connected = true;
        s.input = Input::default();
        s.actions.clear();
        s.last = Some(Instant::now());
        s.connected_at = Some(Instant::now());
        s.ping_ms = None;
        s.tx = Some(tx.clone());
        s.generation
    };
    let _ = tx.send((0x1, format!("h {NAME} {VERSION}").into_bytes()));
    let mut reader = stream;
    let result = (|| -> io::Result<()> {
        loop {
            let (opcode, payload) = read_frame(&mut reader)?;
            if shared().generation != generation {
                return Ok(());
            }
            match opcode {
                0x1 => {
                    let text = String::from_utf8_lossy(&payload);
                    on_message(&text, &tx);
                }
                0x8 => return Ok(()),
                0x9 => {
                    let _ = tx.send((0xA, payload));
                }
                _ => {}
            }
        }
    })();
    let mut s = shared();
    if s.generation == generation {
        s.connected = false;
        s.input = Input::default();
        s.actions.clear();
        s.tx = None;
        s.connected_at = None;
    }
    let _ = tx.send((0x8, Vec::new()));
    result
}

fn on_message(text: &str, tx: &mpsc::Sender<Outgoing>) {
    match text.as_bytes().first() {
        Some(b'i') => {
            if let Some(input) = parse_input(text) {
                let mut s = shared();
                s.input = input;
                s.last = Some(Instant::now());
            }
        }
        // `p <t>`: o celular mede a ida e volta.
        Some(b'p') => {
            shared().last = Some(Instant::now());
            let _ = tx.send((0x1, text.as_bytes().to_vec()));
        }
        // `l <ms>`: o ping que o celular mediu, para o HUD.
        Some(b'l') => {
            let ms = text[1..].trim().parse::<u32>().ok();
            let mut s = shared();
            s.ping_ms = ms;
            s.last = Some(Instant::now());
        }
        _ => {
            if let Some(action) = parse_action(text) {
                let mut s = shared();
                s.last = Some(Instant::now());
                if s.actions.len() < 64 {
                    s.actions.push_back(action);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket (RFC 6455), o mínimo: quadros pequenos, sem fragmentos.

pub fn ws_accept(key: &str) -> String {
    let mut s = key.trim().to_owned();
    s.push_str("258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64(&sha1(s.as_bytes()))
}

fn read_frame(stream: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    stream.read_exact(&mut h)?;
    let opcode = h[0] & 0x0F;
    let masked = h[1] & 0x80 != 0;
    let mut len = u64::from(h[1] & 0x7F);
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b)?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    if len > 64 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok((opcode, payload))
}

fn write_frame(stream: &mut impl Write, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 0xFFFF => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

/// `skins::skin_location`: `skins/` e o SHA-1 das unidades UTF-16 do hash da
/// textura (o fim da URL), o nome com que o jogo guarda a skin no disco.
pub fn skin_location(url: &str) -> String {
    let name = url.rsplit(['/', '\\']).next().unwrap_or(url);
    let hash = name.split(['.', '?', '#']).next().unwrap_or(name);
    let units: Vec<u8> = hash.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::from("skins/");
    for b in sha1(&units) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// O arquivo de uma skin `skins/<sha1>` no cache do jogo, se já foi baixada.
pub fn skin_file(location: &str) -> Option<PathBuf> {
    let digest = location.strip_prefix("skins/")?;
    if digest.len() != 40 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let file = game_home()?.join("assets").join("skins").join(&digest[..2]).join(digest);
    file.is_file().then_some(file)
}

/// O `accounts.json` do launcher (a conta escolhida e a URL da skin dela).
pub fn launcher_accounts() -> Option<String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(a) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(a).join("horizon-launcher"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&h).join(".local").join("share").join("horizon-launcher"));
        dirs.push(PathBuf::from(h).join("Library").join("Application Support").join("horizon-launcher"));
    }
    dirs.iter().find_map(|d| fs::read_to_string(d.join("accounts.json")).ok())
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// QR code (modo byte, correção L, versões 1 a 5: um bloco só), para o HUD.

/// A matriz do QR de `text` (`true` = escuro), ou `None` se não couber.
pub fn qr(text: &str) -> Option<Vec<Vec<bool>>> {
    // (codewords de dados, codewords de correção) por versão, nível L.
    const CAP: [(usize, usize); 5] = [(19, 7), (34, 10), (55, 15), (80, 20), (108, 26)];
    let bytes = text.as_bytes();
    let (version, &(data_len, ec_len)) = CAP
        .iter()
        .enumerate()
        .find(|(_, (d, _))| bytes.len() + 2 <= *d)?;
    let version = version + 1;
    // Bits: modo byte, contagem de 8 bits, os bytes, terminador, completa o byte.
    let mut bits: Vec<bool> = Vec::new();
    let mut push = |v: u32, n: u32| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 == 1));
    push(0b0100, 4);
    push(bytes.len() as u32, 8);
    bytes.iter().for_each(|&b| push(u32::from(b), 8));
    let cap = data_len * 8;
    let term = (cap - bits.len()).min(4);
    bits.extend(std::iter::repeat_n(false, term));
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    let mut data: Vec<u8> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | u8::from(b)))
        .collect();
    let mut pad = [0xECu8, 0x11].into_iter().cycle();
    while data.len() < data_len {
        data.push(pad.next().unwrap_or(0xEC));
    }
    let ec = rs_remainder(&data, &rs_divisor(ec_len));
    data.extend(ec);

    let size = version * 4 + 17;
    let mut m = Matrix {
        size,
        dark: vec![vec![false; size]; size],
        func: vec![vec![false; size]; size],
    };
    m.function_patterns(version);
    m.codewords(&data);
    let best = (0..8u8)
        .min_by_key(|&mask| {
            let mut t = m.clone();
            t.apply_mask(mask);
            t.format_bits(mask);
            t.penalty()
        })
        .unwrap_or(0);
    m.apply_mask(best);
    m.format_bits(best);
    Some(m.dark)
}

thread_local! {
    static QR_CACHE: RefCell<Option<(String, Option<Vec<Vec<bool>>>)>> = const { RefCell::new(None) };
}

/// `qr(text)`, calculado uma vez por texto (o HUD pede a cada frame).
pub fn qr_cached(text: &str) -> Option<Vec<Vec<bool>>> {
    QR_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|(t, _)| t != text) {
            *c = Some((text.to_owned(), qr(text)));
        }
        c.as_ref().and_then(|(_, m)| m.clone())
    })
}

/// Junta os módulos escuros de cada linha em faixas: (coluna, linha, largura).
pub fn qr_runs(grid: &[Vec<bool>]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (y, row) in grid.iter().enumerate() {
        let mut x = 0;
        while x < row.len() {
            if row[x] {
                let start = x;
                while x < row.len() && row[x] {
                    x += 1;
                }
                out.push((start, y, x - start));
            } else {
                x += 1;
            }
        }
    }
    out
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((u32::from(y) >> i) & 1) * u32::from(x);
    }
    z as u8
}

fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    result
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ result.remove(0);
        result.push(0);
        for (r, &d) in result.iter_mut().zip(divisor) {
            *r ^= gf_mul(d, factor);
        }
    }
    result
}

#[derive(Clone)]
struct Matrix {
    size: usize,
    dark: Vec<Vec<bool>>,
    func: Vec<Vec<bool>>,
}

impl Matrix {
    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.dark[y][x] = dark;
        self.func[y][x] = true;
    }

    fn function_patterns(&mut self, version: usize) {
        let n = self.size;
        for i in 0..n {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (n - 4, 3), (3, n - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if (0..n as i32).contains(&x) && (0..n as i32).contains(&y) {
                        let d = dx.abs().max(dy.abs());
                        self.set(x as usize, y as usize, d != 2 && d != 4);
                    }
                }
            }
        }
        if version >= 2 {
            let c = n - 7;
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    let d = dx.abs().max(dy.abs());
                    self.set((c as i32 + dx) as usize, (c as i32 + dy) as usize, d != 1);
                }
            }
        }
        self.format_bits(0);
    }

    fn format_bits(&mut self, mask: u8) {
        let n = self.size;
        let data = (1u32 << 3) | u32::from(mask); // nível L = 01
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = ((data << 10) | rem) ^ 0x5412;
        let bit = |i: u32| (bits >> i) & 1 == 1;
        for i in 0..=5 {
            self.set(8, i as usize, bit(i));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i as usize, 8, bit(i));
        }
        for i in 0..8 {
            self.set(n - 1 - i as usize, 8, bit(i));
        }
        for i in 8..15 {
            self.set(8, n - 15 + i as usize, bit(i));
        }
        self.set(8, n - 8, true);
    }

    fn codewords(&mut self, data: &[u8]) {
        let n = self.size;
        let mut i = 0usize;
        let mut right = n as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..n {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { n - 1 - vert } else { vert };
                    if !self.func[y][x] && i < data.len() * 8 {
                        self.dark[y][x] = (data[i >> 3] >> (7 - (i & 7))) & 1 == 1;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if invert && !self.func[y][x] {
                    self.dark[y][x] = !self.dark[y][x];
                }
            }
        }
    }

    /// Penalidade simplificada (sequências, blocos 2x2 e equilíbrio): basta
    /// para escolher uma máscara boa; qualquer máscara é um QR válido.
    fn penalty(&self) -> usize {
        let n = self.size;
        let mut p = 0;
        for line in 0..n {
            for horizontal in [true, false] {
                let mut run = 1;
                for i in 1..n {
                    let (a, b) = if horizontal {
                        (self.dark[line][i - 1], self.dark[line][i])
                    } else {
                        (self.dark[i - 1][line], self.dark[i][line])
                    };
                    if a == b {
                        run += 1;
                        if run == 5 {
                            p += 3;
                        } else if run > 5 {
                            p += 1;
                        }
                    } else {
                        run = 1;
                    }
                }
            }
        }
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.dark[y][x];
                if c == self.dark[y][x + 1] && c == self.dark[y + 1][x] && c == self.dark[y + 1][x + 1] {
                    p += 3;
                }
            }
        }
        let dark = self.dark.iter().flatten().filter(|&&d| d).count();
        let total = n * n;
        p + (dark * 20).abs_diff(total * 10) / total * 10
    }
}

// ---------------------------------------------------------------------------
// A página do controle (tema Minecraft, deitada, toque com vários dedos).

pub const PAGE: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no,viewport-fit=cover,interactive-widget=resizes-visual">
<meta name="theme-color" content="#3b2a1a">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
<title>PocketPad</title>
<style>
:root{
  --stone:#8b8b8b;--stone-hi:#c6c6c6;--stone-lo:#555;--ink:#fff;--shadow:#3f3f3f;
  --sel:#7a8cc2;--grass:#5d9c3f;--grass-hi:#7fc650;--dirt:#79553a;--dirt-lo:#593d29;
  --red:#d24a3c;--gold:#f5c542;--panel:rgba(0,0,0,.55);
  --gui:#c6c6c6;--slot:#8b8b8b;--slot-hi:#fff;--slot-lo:#373737;
  --touch:rgba(110,110,110,.42);--touch-on:rgba(215,215,215,.62);--touch-edge:rgba(255,255,255,.55);
  --sl:env(safe-area-inset-left);--sr:env(safe-area-inset-right);--sb:env(safe-area-inset-bottom);--st:env(safe-area-inset-top);
}
*{box-sizing:border-box;-webkit-tap-highlight-color:transparent;user-select:none;-webkit-user-select:none;-webkit-touch-callout:none}
html,body{margin:0;height:100%;overflow:hidden;touch-action:none;overscroll-behavior:none}
body{
  background-color:var(--dirt);
  background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='16' height='16' shape-rendering='crispEdges'%3E%3Crect width='16' height='16' fill='%2379553a'/%3E%3Cg fill='%23593d29'%3E%3Crect x='1' y='2' width='2' height='1'/%3E%3Crect x='9' y='1' width='1' height='2'/%3E%3Crect x='5' y='6' width='2' height='1'/%3E%3Crect x='12' y='5' width='2' height='2'/%3E%3Crect x='2' y='11' width='1' height='2'/%3E%3Crect x='7' y='12' width='2' height='1'/%3E%3Crect x='13' y='13' width='1' height='1'/%3E%3C/g%3E%3Cg fill='%23966c4a'%3E%3Crect x='4' y='3' width='1' height='1'/%3E%3Crect x='11' y='9' width='2' height='1'/%3E%3Crect x='3' y='8' width='1' height='1'/%3E%3Crect x='14' y='2' width='1' height='1'/%3E%3Crect x='9' y='14' width='1' height='1'/%3E%3C/g%3E%3C/svg%3E");
  background-size:64px 64px;image-rendering:pixelated;
  color:var(--ink);font:bold 13px/1.1 ui-monospace,"Courier New",monospace;
}
/* A terra de verdade (textura do jogo), escurecida como o fundo dos menus; o desenho acima fica de reserva. */
body{background:linear-gradient(rgba(0,0,0,.28),rgba(0,0,0,.28)),url(/tex/block/dirt.png) 0 0/64px 64px,var(--dirt);image-rendering:pixelated}
/* Texto na fonte do jogo: canvas desenhado por mc(); antes de a fonte chegar, texto comum. */
.mc{display:inline-block;line-height:0;vertical-align:middle}
.mc canvas{image-rendering:pixelated;display:block}
/* Em cima, a lateral do bloco de grama, na mesma grade de 64 px da terra: emenda sem costura. */
#grass{position:fixed;left:0;right:0;top:0;height:64px;background:linear-gradient(rgba(0,0,0,.28),rgba(0,0,0,.28)),url(/tex/block/grass_block_side.png) 0 0/64px 64px repeat-x;image-rendering:pixelated;pointer-events:none;z-index:0}
/* Botões de pedra (menus, inventário) */
.btn{
  position:absolute;display:flex;align-items:center;justify-content:center;gap:6px;
  background:var(--stone);border:2px solid #000;color:var(--ink);padding:0 10px;
  box-shadow:inset 2px 2px 0 var(--stone-hi),inset -2px -2px 0 var(--stone-lo);font:inherit;
}
.btn svg,.tb svg{width:22px;height:22px;shape-rendering:crispEdges;flex:none}
.btn.on{background:var(--sel);box-shadow:inset 2px 2px 0 #a8b8ee,inset -2px -2px 0 #4a5a8c}
.btn.lock{background:#6d8f4e;box-shadow:inset 2px 2px 0 #9ccc6f,inset -2px -2px 0 #3f5a2b}
/* Botões de toque do Bedrock: translúcidos, borda clara */
.tb{position:absolute;display:flex;align-items:center;justify-content:center;background:var(--touch);border:2px solid var(--touch-edge);border-radius:3px;padding:0}
.tb.on{background:var(--touch-on)}
.tb.lock{background:rgba(127,198,80,.6)}
/* Olhar: a tela toda por trás dos botões */
#look{position:absolute;inset:0;z-index:1}
/* O analógico da câmera: aparece onde o dedo encosta. */
#lstick{position:absolute;width:132px;height:132px;margin:-66px 0 0 -66px;display:none;pointer-events:none;
  background:rgba(0,0,0,.32);border:2px solid var(--touch-edge);border-radius:50%}
#lstick .knob{position:absolute;left:50%;top:50%;width:56px;height:56px;margin:-28px 0 0 -28px;border-radius:50%;
  background:radial-gradient(circle at 35% 35%,rgba(255,255,255,.85),rgba(200,200,200,.6) 45%,rgba(120,120,120,.55));border:2px solid rgba(255,255,255,.7);box-shadow:0 2px 6px rgba(0,0,0,.4)}
#hint{position:absolute;left:50%;top:40%;transform:translateX(-50%);opacity:.6;pointer-events:none;text-align:center}
#hint .mc{margin:3px 0}
/* D-pad */
#dpad{position:absolute;left:calc(max(14px,var(--sl)) + 8px);bottom:calc(max(10px,var(--sb)) + 8px);width:186px;height:186px;z-index:2;touch-action:none}
#dpad .tb{width:60px;height:60px;pointer-events:none}
#dpad .diag{display:none;opacity:.85}
#dpad.fwd .diag{display:flex}
/* Pular */
#jump{right:calc(max(14px,var(--sr)) + 22px);bottom:calc(max(10px,var(--sb)) + 62px);width:84px;height:84px;z-index:2}
#use{right:calc(max(14px,var(--sr)) + 22px);bottom:calc(max(10px,var(--sb)) + 158px);width:84px;height:62px;z-index:2}
#use svg{width:30px;height:30px}
body.inv #use,body.menu #use{display:none}
/* Modelos 3D dos itens e do personagem (CSS 3D). */
.m3w{position:absolute;left:50%;top:50%;width:0;height:0;transform-style:preserve-3d;pointer-events:none}
.m3{position:absolute;left:0;top:0;transform-style:preserve-3d}
.mf{position:absolute;left:0;top:0;transform-origin:0 0;backface-visibility:hidden;overflow:hidden}
.mi{position:absolute;background-repeat:no-repeat;image-rendering:pixelated}
.pbox{position:absolute;overflow:hidden;pointer-events:none}
/* O painel da direita no estilo do tooltip do jogo. */
.tipbox{background:rgba(16,0,16,.94);border:2px solid #100010;box-shadow:inset 0 0 0 2px #3a0f8f;padding:8px;display:flex;flex-direction:column;gap:6px;align-items:flex-start;min-height:56px;overflow:hidden}
.tipbox .box{position:relative;width:48px;height:48px;flex:none}
.tipbox .tx{display:flex;flex-direction:column;gap:4px;min-width:0;max-width:100%}
#jump svg{width:40px;height:40px}
/* Topo */
#top{position:absolute;left:0;right:0;top:calc(14px + var(--st));display:flex;justify-content:center;gap:8px;z-index:3;pointer-events:none}
#top .tb{position:relative;width:46px;height:40px;pointer-events:auto}
#st{position:absolute;left:max(14px,var(--sl));top:calc(16px + var(--st));background:rgba(0,0,0,.45);padding:6px 8px;z-index:3;display:flex;align-items:center;gap:6px}
#st i{display:inline-block;width:8px;height:8px;background:var(--red);border:1px solid #000}
#st.ok i{background:var(--grass-hi)}
/* Hotbar e vida */
#hud{position:absolute;left:50%;bottom:max(4px,var(--sb));transform:translateX(-50%);display:flex;flex-direction:column;align-items:center;gap:3px;z-index:2}
#hearts{display:flex;gap:1px;height:14px;align-self:flex-start;margin-left:3px}
#hearts svg{width:14px;height:14px;shape-rendering:crispEdges}
#barrow{display:flex;align-items:stretch}
/* A hotbar do jogo (hud/hotbar.png, 182x22: slots de 20 px a partir de 1, 1) em escala 2,
   e a seleção (hud/hotbar_selection.png, 24x23) 1 px para fora do slot. */
#hotbar{display:flex;padding:2px;width:364px;height:44px;box-sizing:border-box;background:url(/tex/gui/sprites/hud/hotbar.png) 0 0/364px 44px no-repeat;image-rendering:pixelated}
.hs{position:relative;width:40px;height:40px}
.hs.sel::after{content:"";position:absolute;left:-4px;top:-4px;width:48px;height:46px;background:url(/tex/gui/sprites/hud/hotbar_selection.png) 0 0/48px 46px no-repeat;image-rendering:pixelated;pointer-events:none;z-index:1}
#more{position:relative;width:38px;height:auto;margin-left:4px;background:rgba(0,0,0,.55);border:2px solid #000;box-shadow:0 0 0 2px #555}
#more.on{background:rgba(150,150,150,.6)}
body.inv #hud,body.inv #dpad,body.inv #jump,body.inv #top,body.inv #look,body.inv #hint{display:none}
body.menu #hud,body.menu #hint{display:none}
/* Ícones */
.ico{position:absolute;inset:8%;pointer-events:none}
.flat{position:absolute;inset:0;background-size:100% auto;background-repeat:no-repeat;image-rendering:pixelated}
.cube{position:absolute;left:50%;top:50%;transform-style:preserve-3d}
.cube b{position:absolute;inset:0;background-size:100% auto;background-repeat:no-repeat;image-rendering:pixelated;backface-visibility:hidden}
.tint{background-blend-mode:multiply}
.txt{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;overflow:hidden}
.cnt{position:absolute;right:0;bottom:0;pointer-events:none;z-index:2}
/* Inventário */
#inv{position:fixed;inset:0;display:none;z-index:4}
body.inv #inv{display:block}
#invSide{position:absolute;left:max(10px,var(--sl));top:20px;bottom:max(10px,var(--sb));width:174px;display:flex;flex-direction:column;gap:8px}
#invSide .btn{position:relative;height:44px;width:100%;justify-content:flex-start}
#invSide .tabs{display:flex;gap:8px}
#invSide .tabs .btn{width:83px;justify-content:center}
#winWrap{position:absolute;top:18px;bottom:max(8px,var(--sb));left:calc(max(10px,var(--sl)) + 182px);right:calc(max(10px,var(--sr)) + 196px);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:2px}
/* Abas do criativo: as de cima e as de baixo da janela, como no PC. */
.ctabs{display:none;gap:3px;justify-content:center}
.ct{position:relative;width:38px;height:34px;background:#a8a8a8;border:2px solid #000;box-shadow:inset 2px 2px 0 #e0e0e0,inset -2px -2px 0 #6b6b6b;padding:0}
.ct.sel{background:var(--gui);box-shadow:inset 2px 2px 0 #fff,inset -2px -2px 0 #8b8b8b;outline:2px solid #fff;outline-offset:-5px}
.cre-only{display:none!important}
body.cre .cre-only{display:flex!important}
body.cre .no-cre{display:none!important}
#win{position:relative;background:var(--gui);border:2px solid #000;box-shadow:inset 2px 2px 0 #fff,inset -2px -2px 0 #555;border-radius:3px}
#winTitle{position:absolute;left:8px;top:6px}
.sl{position:absolute;background:var(--slot);box-shadow:inset 2px 2px 0 var(--slot-lo),inset -2px -2px 0 var(--slot-hi)}
/* Janela com a textura do próprio jogo (inventário, bancada, criativo): igual ao PC. */
/* A área que centraliza a janela é transparente: deixa o toque passar para as
   abas não selecionadas, que ficam atrás da janela (como no PC). */
#winWrap{pointer-events:none}
#win,#win *{pointer-events:auto}
#win .lbl,#win .tico,#win .scr .thumb{pointer-events:none}
#win.skin{border:0;box-shadow:none;border-radius:0;image-rendering:pixelated}
#win.skin .sl{background:transparent;box-shadow:none}
#win.skin .sl .ico{inset:5.5%}
#win.skin .sl.hit{background:rgba(255,255,255,.5)}
#win.skin #winTitle{display:none}
.tabx{position:absolute;background-size:100% 100%;image-rendering:pixelated;cursor:pointer}
.tico{position:absolute;pointer-events:none}
.tico .ico{inset:0}
.lbl{position:absolute;pointer-events:none}
.scr{position:absolute;touch-action:none}
.scr .thumb{position:absolute;left:0;background-size:100% 100%;image-rendering:pixelated;pointer-events:none}
.bin{position:absolute}
.bin.on{background:rgba(255,80,80,.45)}
#trash.on{filter:brightness(1.4) sepia(1) hue-rotate(-30deg) saturate(3)}
.sl.hit{background:#b4b4ff}
#float{position:fixed;left:0;top:0;display:none;pointer-events:none;z-index:6;transform:translate(-50%,-70%);filter:drop-shadow(0 3px 3px rgba(0,0,0,.5))}
body.inv.carry.touching #float{display:block}
.sl.big{box-shadow:inset 2px 2px 0 var(--slot-lo),inset -2px -2px 0 var(--slot-hi),0 0 0 3px var(--slot)}
.arrow{position:absolute;pointer-events:none}
#invInfo{position:absolute;right:max(10px,var(--sr));top:20px;width:186px;display:flex;flex-direction:column;gap:8px}
.card{background:var(--panel);border:2px solid #000;padding:8px;display:flex;flex-direction:column;gap:6px}
#carried .box{position:relative;width:52px;height:52px;background:var(--slot);box-shadow:inset 2px 2px 0 var(--slot-lo),inset -2px -2px 0 var(--slot-hi)}
/* Chat */
#chat{position:fixed;inset:0;display:none;z-index:6;background:rgba(10,8,6,.88);flex-direction:column;padding:calc(14px + var(--st)) max(12px,var(--sr)) max(10px,var(--sb)) max(12px,var(--sl))}
body.chat #chat{display:flex}
#chatLog{flex:1;overflow-y:auto;touch-action:pan-y;padding:8px;background:rgba(0,0,0,.45);border:2px solid #000;display:flex;flex-direction:column;gap:3px}
#chatForm{display:flex;gap:8px;margin-top:8px}
#chatIn{flex:1;font:16px ui-monospace,"Courier New",monospace;padding:8px 10px;background:#000;color:#fff;border:2px solid #a0a0a0;outline:none;user-select:text;-webkit-user-select:text}
#chatForm .btn{position:relative;height:42px}
/* Ajustes */
#cfg{position:fixed;inset:0;display:none;z-index:7;background:rgba(10,8,6,.86);align-items:center;justify-content:center}
body.cfg #cfg{display:flex}
/* Editar o HUD: uma camada por cima de tudo pega os toques; as peças ganham contorno. */
#edit{position:fixed;inset:0;z-index:8;display:none;touch-action:none}
body.edit #edit{display:block}
body.edit #hint{visibility:hidden}
body.edit :is(#dpad,#jump,#use,#hud,#top,#st){outline:2px dashed rgba(255,255,255,.55);outline-offset:3px}
body.edit :is(#dpad,#jump,#use,#hud,#top,#st).esel{outline:2px solid #f5c542}
#ebar{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);display:flex;flex-direction:column;gap:6px;padding:10px 12px;width:min(300px,60vw);background:rgba(10,8,6,.85);border:2px solid #000;box-shadow:0 0 0 2px #555}
#ebar input{width:100%;accent-color:#7fc650}
#ebar .row{display:flex;gap:8px}
#ebar .btn{position:relative;height:38px;flex:1}
#cfg .card{width:min(420px,90vw);gap:12px;padding:16px}
#cfg input{width:100%;accent-color:#7fc650}
#cfg .row{display:flex;gap:8px}
#cfg .btn{position:relative;height:42px;flex:1}
/* Capas */
#load{position:fixed;inset:0;display:none;flex-direction:column;align-items:center;justify-content:center;gap:14px;z-index:8;text-align:center;padding:24px;
  background:linear-gradient(rgba(0,0,0,.45),rgba(0,0,0,.45)),url(/tex/block/dirt.png) 0 0/64px 64px,var(--dirt);image-rendering:pixelated}
body.loading #load{display:flex}
#loadBar{width:min(440px,72vw);height:14px;border:2px solid #fff;padding:2px;box-sizing:border-box}
#loadBar i{display:block;height:100%;width:0;background:#fff}
#over{position:fixed;inset:0;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:16px;background:rgba(20,14,8,.86);z-index:9;text-align:center;padding:24px}
#over .btn{position:static;height:48px;padding:0 30px}
#code{width:min(260px,70vw);font:22px ui-monospace,"Courier New",monospace;text-align:center;letter-spacing:3px;padding:8px 10px;background:#000;color:#fff;border:2px solid #a0a0a0;outline:none;user-select:text;-webkit-user-select:text}
#rot{position:fixed;inset:0;display:none;align-items:center;justify-content:center;flex-direction:column;gap:14px;background:#2a1d12;z-index:10;text-align:center;padding:24px}
@media (orientation:portrait){#rot{display:flex}}
</style>
</head>
<body>
<div id="grass"></div>
<div id="look"><div id="lstick"><div class="knob"></div></div></div>
<div id="hint"><div data-tk="look" data-t="" data-s="2"></div><div data-tk="tapuse" data-t="" data-s="2"></div></div>
<div id="st"><i></i><span id="stt" data-t="" data-s="2"></span></div>
<div id="top">
  <button class="tb" data-b="9" data-ico="pause" aria-label="Pause" data-al="pause"></button>
  <button class="tb" id="chatBtn" data-ico="chat" aria-label="Chat" data-al="chat"></button>
  <button class="tb" data-b="12" data-ico="eye" aria-label="Camera" data-al="camera"></button>
  <button class="tb" id="cfgBtn" data-ico="gear" aria-label="Settings" data-al="settings"></button>
</div>
<div id="dpad"></div>
<button class="tb" id="jump" data-ico="jump" aria-label="Jump" data-al="jump"></button>
<button class="tb" id="use" data-ico="use" aria-label="Use" data-al="use"></button>
<div id="hud">
  <div id="hearts"></div>
  <div id="barrow"><div id="hotbar"></div><button class="tb" id="more" data-ico="dots" aria-label="Inventory" data-al="inventory"></button></div>
</div>
<div id="inv">
  <div id="invSide">
    <button class="btn" id="invClose" data-ico="back"><span data-tk="close" data-t="" data-s="2"></span></button>
    <button class="btn" id="modeShift" data-ico="fast"><span data-tk="fast" data-t="" data-s="2"></span></button>
    <button class="btn" id="modeHalf" data-ico="half"><span data-tk="half" data-t="" data-s="2"></span></button>
    <div class="tabs no-cre"><button class="btn" id="tabL" data-ico="left"></button><button class="btn" id="tabR" data-ico="right"></button></div>
    <button class="btn cre-only" id="trash" data-ico="trash"><span data-tk="trash" data-t="" data-s="2"></span></button>
  </div>
  <div id="winWrap"><div id="ctTop" class="ctabs"></div><div id="win"><div id="winTitle"></div></div><div id="ctBot" class="ctabs"></div></div>
  <div id="invInfo"><div class="tipbox" id="tipbox"><div class="box"></div><div class="tx"><div class="nm"></div><div class="sub"></div></div></div></div>
</div>
<div id="chat">
  <div id="chatLog"></div>
  <form id="chatForm" autocomplete="off">
    <input id="chatIn" maxlength="256" placeholder="" enterkeyhint="send">
    <button class="btn" type="submit" data-ico="send"><span data-tk="send" data-t="" data-s="2"></span></button>
    <button class="btn" type="button" id="chatClose" data-ico="back"><span data-g="gui.back" data-tk="back" data-t="" data-s="2"></span></button>
  </form>
</div>
<div id="cfg"><div class="card">
  <div data-g="options.title" data-tk="settings" data-t="" data-s="3" data-c="#f5c542"></div>
  <div id="sensT"></div>
  <input type="range" id="sens" min="0.3" max="3" step="0.1">
  <div class="row"><button class="btn" id="editBtn"><span data-tk="editHud" data-t="" data-s="2"></span></button></div>
  <div class="row"><button class="btn" id="cfgOk"><span data-g="gui.done" data-tk="done" data-t="" data-s="2"></span></button></div>
</div></div>
<div id="edit"><div id="ebar">
  <div data-tk="editTip" data-t="" data-s="2" data-c="#a8a8a8"></div>
  <div data-tk="size" data-t="" data-s="2"></div>
  <input type="range" id="eSize" min="0.5" max="2" step="0.05">
  <div data-tk="opacity" data-t="" data-s="2"></div>
  <input type="range" id="eOp" min="0.2" max="1" step="0.05">
  <div class="row"><button class="btn" id="eReset"><span data-tk="reset" data-t="" data-s="2"></span></button><button class="btn" id="eDone"><span data-g="gui.done" data-tk="done" data-t="" data-s="2"></span></button></div>
</div></div>
<div id="float"></div>
<div id="load">
  <div data-tk="loading" data-s="3" data-c="#ffffff" data-t=""></div>
  <div id="loadStep" data-s="2" data-c="#a8a8a8"></div>
  <div id="loadBar"><i></i></div>
</div>
<div id="over">
  <div data-t="PocketPad" data-s="4" data-c="#f5c542"></div>
  <div id="overt"></div>
  <input id="code" type="text" inputmode="text" autocomplete="off" autocapitalize="off" spellcheck="false" maxlength="16" placeholder="a1b2c3d4" style="display:none">
  <button class="btn" id="go"><span data-tk="play" data-t="" data-s="3"></span></button>
</div>
<div id="rot"><div data-tk="rotate" data-t="" data-s="4" data-c="#f5c542"></div><div data-tk="lying" data-t="" data-s="2"></div></div>
<script>
"use strict";
// Os textos do PocketPad em cada idioma (o jogo traduz o resto: ver setLang).
const TX={
  pt:{loading:"Carregando recursos",ldFont:"Fonte e idioma",ldCat:"Menu do criativo",ldModels:"Modelos dos itens",ldItems:"Montando os itens",ldTex:"Texturas",look:"Arraste para olhar",tapuse:"Toque: usar · Segure: quebrar",codeAsk:"Digite o código da sala. No jogo: Opções > Mods > PocketPad > Conectar celular.",codeBad:"Código errado. Confira em Opções > Mods > PocketPad.",editHud:"Editar HUD",editTip:"Toque numa peça e arraste.",size:"Tamanho",opacity:"Opacidade",reset:"Restaurar",connecting:"Conectando",connected:"Conectado",offline:"Sem conexão",
    close:"Fechar",fast:"Mover rápido",half:"Metade / um",trash:"Lixeira",send:"Enviar",back:"Voltar",settings:"Ajustes",done:"Pronto",
    msg:"Mensagem ou /comando",play:"Jogar",rotate:"Gire o celular",lying:"O controle funciona deitado.",
    taken:"Outro celular assumiu",takenLong:"Outro celular assumiu o controle. Toque para retomar.",
    start:"Toque para começar. O celular vira o controle do Horizon: olhe para a tela do PC.",qr:"Abra pelo QR que aparece na tela do jogo.",
    tip1:"Toque: pegar e colocar.",tip2:"Segure e arraste para mover.",hand:"Na mão",items:"{n} itens",sens:"Sensibilidade do olhar",crafting:"Fabricação",inventory:"Inventário",pause:"Pausa",chat:"Chat",camera:"Câmera",jump:"Pular",use:"Usar"},
  en:{loading:"Loading resources",ldFont:"Font and language",ldCat:"Creative menu",ldModels:"Item models",ldItems:"Building the items",ldTex:"Textures",look:"Drag to look",tapuse:"Tap: use · Hold: break",codeAsk:"Type the room code. In the game: Options > Mods > PocketPad > Connect phone.",codeBad:"Wrong code. Check it in Options > Mods > PocketPad.",editHud:"Edit HUD",editTip:"Tap a piece and drag it.",size:"Size",opacity:"Opacity",reset:"Reset",connecting:"Connecting",connected:"Connected",offline:"No connection",
    close:"Close",fast:"Quick move",half:"Half / one",trash:"Trash",send:"Send",back:"Back",settings:"Settings",done:"Done",
    msg:"Message or /command",play:"Play",rotate:"Rotate your phone",lying:"The controller works in landscape.",
    taken:"Another phone took over",takenLong:"Another phone took over the controller. Tap to take it back.",
    start:"Tap to start. Your phone becomes Horizon's controller: look at the PC screen.",qr:"Open it from the QR code on the game screen.",
    tip1:"Tap: pick up and place.",tip2:"Hold and drag to move.",hand:"In hand",items:"{n} items",sens:"Look sensitivity",crafting:"Crafting",inventory:"Inventory",pause:"Pause",chat:"Chat",camera:"Camera",jump:"Jump",use:"Use"},
  es:{loading:"Cargando recursos",ldFont:"Fuente e idioma",ldCat:"Menú del creativo",ldModels:"Modelos de los objetos",ldItems:"Montando los objetos",ldTex:"Texturas",look:"Arrastra para mirar",tapuse:"Toca: usar · Mantén: romper",codeAsk:"Escribe el código de la sala. En el juego: Opciones > Mods > PocketPad > Conectar el móvil.",codeBad:"Código incorrecto. Revísalo en Opciones > Mods > PocketPad.",editHud:"Editar HUD",editTip:"Toca una pieza y arrástrala.",size:"Tamaño",opacity:"Opacidad",reset:"Restablecer",connecting:"Conectando",connected:"Conectado",offline:"Sin conexión",
    close:"Cerrar",fast:"Mover rápido",half:"Mitad / uno",trash:"Papelera",send:"Enviar",back:"Volver",settings:"Ajustes",done:"Listo",
    msg:"Mensaje o /comando",play:"Jugar",rotate:"Gira el móvil",lying:"El mando funciona en horizontal.",
    taken:"Otro móvil tomó el control",takenLong:"Otro móvil tomó el control. Toca para recuperarlo.",
    start:"Toca para empezar. El móvil es el mando de Horizon: mira la pantalla del PC.",qr:"Ábrelo desde el QR de la pantalla del juego.",
    tip1:"Toca: coger y colocar.",tip2:"Mantén y arrastra para mover.",hand:"En la mano",items:"{n} objetos",sens:"Sensibilidad de la vista",crafting:"Fabricación",inventory:"Inventario",pause:"Pausa",chat:"Chat",camera:"Cámara",jump:"Saltar",use:"Usar"},
};
// Inglês até o jogo mandar o idioma dele (o último que ele mandou fica guardado).
let LANG=(()=>{try{const v=localStorage.getItem("cc.glang");if(v)return v;}catch(e){}return"en_us";})();
function tr(k,n){const d=TX[LANG.slice(0,2)]||TX.en;const t=d[k]??TX.en[k]??k;return n==null?t:t.replace("{n}",n);}
const $=id=>document.getElementById(id);
function buzz(ms){try{navigator.vibrate&&navigator.vibrate(ms)}catch(e){}}
function store(k,v){try{if(v===undefined)return localStorage.getItem("cc."+k);localStorage.setItem("cc."+k,v);}catch(e){return null;}}

// ---- Fonte do jogo -----------------------------------------------------------
// Os bitmaps de font/include/default.json, medidos como o jogo mede (largura =
// última coluna com pixel + 1), desenhados em canvas com a sombra do Minecraft.
const FONT={glyphs:new Map(),ready:false};
async function loadFont(){
  try{
    const def=await (await fetch("/font.json")).json();
    for(const p of def.providers||[]){
      if(p.type!=="bitmap")continue;
      const file=p.file.replace(/^minecraft:/,"");
      const img=new Image();img.src="/tex/"+file;await img.decode();
      const rows=p.chars.map(r=>[...r]),cols=Math.max(...rows.map(r=>r.length));
      const cw=img.width/cols,ch=img.height/rows.length,h=p.height||8,asc=p.ascent??7,s=h/ch;
      const c=document.createElement("canvas");c.width=img.width;c.height=img.height;
      const g=c.getContext("2d",{willReadFrequently:true});g.drawImage(img,0,0);
      const data=g.getImageData(0,0,img.width,img.height).data;
      rows.forEach((row,ry)=>row.forEach((chr,rx)=>{
        if(chr==="\u0000"||chr===" "||FONT.glyphs.has(chr))return;
        let w=0;
        for(let x=cw-1;x>=0&&!w;x--)for(let y=0;y<ch;y++){if(data[((ry*ch+y)*img.width+rx*cw+x)*4+3]>0){w=x+1;break;}}
        FONT.glyphs.set(chr,{img:c,sx:rx*cw,sy:ry*ch,cw,ch,s,asc,adv:Math.floor(0.5+w*s)+1});
      }));
    }
    FONT.ready=FONT.glyphs.size>0;
  }catch(e){FONT.ready=false;}
  if(FONT.ready)document.querySelectorAll("[data-t]").forEach(el=>mc(el,el.dataset.t));
}
function advance(chr){if(chr===" ")return 4;const g=FONT.glyphs.get(chr)||FONT.glyphs.get("?");return g?g.adv:6;}
function measure(text){let w=0;for(const c of text)w+=advance(c);return w;}
// Quebra em linhas que cabem em `max` unidades da fonte.
function wrap(text,max){if(!max)return[text];const out=[];let line="";
  for(const word of text.split(" ")){const t=line?line+" "+word:word;if(line&&measure(t)>max){out.push(line);line=word;}else line=t;}
  out.push(line);return out;}
const DPR=Math.min(3,Math.max(1,Math.round(window.devicePixelRatio||1)));
function shade(hex){const n=parseInt(hex.slice(1),16);const f=v=>Math.floor(v/4).toString(16).padStart(2,"0");return"#"+f(n>>16&255)+f(n>>8&255)+f(n&255);}
// Escreve `text` em `el` na fonte do jogo. data-s: escala; data-c: cor; data-w: largura máxima (px).
function mc(el,text,opt){
  text=String(text??"");el.dataset.t=text;
  let scale=+(opt&&opt.s||el.dataset.s||2);const color=(opt&&opt.c)||el.dataset.c||"#ffffff";
  const shadow=!((opt&&opt.noShadow)||el.dataset.ns);const maxPx=+(opt&&opt.w||el.dataset.w||0);
  // Guarda as opções: quando a fonte chega, o texto é redesenhado igual.
  el.dataset.s=scale;el.dataset.c=color;if(!shadow)el.dataset.ns="1";if(maxPx)el.dataset.w=maxPx;if(opt&&opt.fit)el.dataset.fit="1";
  if(!FONT.ready){el.textContent=text;el.style.color=color;el.style.fontSize=(scale*6)+"px";return;}
  el.textContent="";el.classList.add("mc");el.style.color="";el.style.fontSize="";
  let lines=wrap(text,maxPx?Math.floor(maxPx/scale)-1:0);
  // fit: uma palavra mais larga que a caixa diminui a letra (2 → 1,5 → 1).
  if((opt&&opt.fit||el.dataset.fit)&&maxPx)while(scale>1&&Math.max(...lines.map(measure))*scale>maxPx){scale=Math.max(1,scale-0.5);lines=wrap(text,Math.floor(maxPx/scale)-1);}
  const W=Math.max(1,...lines.map(measure))+1,H=lines.length*10;
  const S=scale*DPR,cv=document.createElement("canvas");cv.width=W*S;cv.height=H*S;cv.style.width=W*scale+"px";cv.style.height=H*scale+"px";
  const g=cv.getContext("2d");g.imageSmoothingEnabled=false;
  const mask=document.createElement("canvas");mask.width=cv.width;mask.height=cv.height;const m=mask.getContext("2d");m.imageSmoothingEnabled=false;
  lines.forEach((ln,i)=>{let x=0;for(const c of ln){if(c!==" "){const gl=FONT.glyphs.get(c)||FONT.glyphs.get("?");
    if(gl)m.drawImage(gl.img,gl.sx,gl.sy,gl.cw,gl.ch,x*S,(i*10+7-gl.asc)*S,gl.cw*gl.s*S,gl.ch*gl.s*S);}x+=advance(c);}});
  const tint=c=>{const t=document.createElement("canvas");t.width=mask.width;t.height=mask.height;const k=t.getContext("2d");
    k.drawImage(mask,0,0);k.globalCompositeOperation="source-in";k.fillStyle=c;k.fillRect(0,0,t.width,t.height);return t;};
  if(shadow)g.drawImage(tint(shade(color)),S,S);
  g.drawImage(tint(color),0,0);el.appendChild(cv);
}
document.querySelectorAll("[data-t]").forEach(el=>mc(el,el.dataset.t));
const FONTP=loadFont();

// ---- Ícones de pixel (8x8) ---------------------------------------------------
const ICO={
  up:["...ww...","..wwww..",".wwwwww.","wwwwwwww","...ww...","...ww...","...ww...","...ww..."],
  down:["...ww...","...ww...","...ww...","...ww...","wwwwwwww",".wwwwww.","..wwww..","...ww..."],
  left:["...w....","..ww....",".wwwwwww","wwwwwwww","wwwwwwww",".wwwwwww","..ww....","...w...."],
  right:["....w...","....ww..","wwwwwww.","wwwwwwww","wwwwwwww","wwwwwww.","....ww..","....w..."],
  ul:["wwwww...","wwww....","wwww....","ww.ww...","w...ww..",".....ww.","......ww",".......w"],
  ur:["...wwwww","....wwww","....wwww","...ww.ww","..ww...w",".ww.....","ww......","w......."],
  sneak:["........","........","..wwww..",".w....w.",".w....w.","..wwww..","........","........"],
  jump:["...ww...","..wwww..",".ww..ww.","ww....ww","...ww...","...ww...","...ww...","........"],
  dots:["........","........","........","w..w..w.","w..w..w.","........","........","........"],
  pause:["........",".ww..ww.",".ww..ww.",".ww..ww.",".ww..ww.",".ww..ww.",".ww..ww.","........"],
  eye:["........","..wwww..",".w....w.","w..bb..w","w..bb..w",".w....w.","..wwww..","........"],
  chat:["wwwwwwww","w......w","w.ww.w.w","w......w","wwwwwwww","..ww....",".w......","........"],
  gear:["...ww...",".w.ww.w.","..wwww..","wwww.www","www.wwww","..wwww..",".w.ww.w.","...ww..."],
  back:["..w.....",".ww.....","wwwwww..",".ww...w.","..w...w.","......w.","..wwww..","........"],
  fast:["w...w...","ww..ww..","www.www.","wwwwwwww","wwwwwwww","www.www.","ww..ww..","w...w..."],
  half:["wwwwwwww","w...w..w","w...w..w","w...w..w","w...w..w","w...w..w","w...w..w","wwwwwwww"],
  send:["w.......","ww......","www.....","wwwwwww.","wwwwwww.","www.....","ww......","w......."],
  trash:["..wwww..","wwwwwwww","........",".w.ww.w.",".w.ww.w.",".w.ww.w.",".w.ww.w.","..wwww.."],
};
const PAL={w:"#ffffff",b:"#7a5230"};
function svg(name){const g=ICO[name];if(!g)return"";let r="";g.forEach((row,y)=>[...row].forEach((ch,x)=>{if(ch!=="."&&PAL[ch])r+=`<rect x="${x}" y="${y}" width="1" height="1" fill="${PAL[ch]}"/>`}));return`<svg viewBox="0 0 8 8">${r}</svg>`;}
document.querySelectorAll("[data-ico]").forEach(el=>{el.insertAdjacentHTML("afterbegin",svg(el.dataset.ico));});

// ---- Estado do controle ------------------------------------------------------
// Bits = BUTTONS do mod: south east west north lb rb lt rt select start ls rs up down left right.
const B={south:0,east:1,west:2,north:3,start:9,ls:10,rs:11,up:12,down:13,left:14,right:15};
const state={lx:0,ly:0,rx:0,ry:0,lt:0,rt:0,btn:0};
let dirty=true,mode="",target=null;
function setBit(i,on){state.btn=on?(state.btn|(1<<i)):(state.btn&~(1<<i));dirty=true;}
function pulse(i,ms=90){setBit(i,true);setTimeout(()=>setBit(i,false),ms);}
function trig(which,ms=110){state[which]=1;dirty=true;setTimeout(()=>{state[which]=0;dirty=true;},ms);}
function hold(el,down,up){
  el.addEventListener("pointerdown",e=>{e.preventDefault();e.stopPropagation();el.setPointerCapture(e.pointerId);down(e);});
  const end=()=>up();
  el.addEventListener("pointerup",end);el.addEventListener("pointercancel",end);el.addEventListener("lostpointercapture",end);
}
// Topo: pausa e câmera (bits).
document.querySelectorAll("#top .tb[data-b]").forEach(el=>{const b=+el.dataset.b;
  hold(el,()=>{el.classList.add("on");setBit(b,true);buzz(10);},()=>{if(el.classList.contains("on")){el.classList.remove("on");setBit(b,false);}});});

// ---- D-pad (andar; nos menus, navegar) -----------------------------------------
// Como o do Bedrock: o dedo desliza entre as setas; com ↑ seguro aparecem as
// diagonais; o meio agacha; dois toques em ↑ correm.
const dpad=$("dpad"),CELL=62;
const CELLS={ul:[0,0],up:[1,0],ur:[2,0],left:[0,1],mid:[1,1],right:[2,1],down:[1,2]};
const cellEl={};
for(const[k,[cx,cy]]of Object.entries(CELLS)){const el=document.createElement("div");el.className="tb"+(k==="ul"||k==="ur"?" diag":"");
  el.style.left=cx*CELL+"px";el.style.top=cy*CELL+"px";el.innerHTML=svg(k==="mid"?"sneak":k);dpad.appendChild(el);cellEl[k]=el;}
let dpId=null,dpCell=null,lastUp=0,sneaking=false;
function cellAt(e){const r=dpad.getBoundingClientRect(),c=CELL*(r.width/(dpad.offsetWidth||1)),x=Math.floor((e.clientX-r.left)/c),y=Math.floor((e.clientY-r.top)/c);
  for(const[k,[cx,cy]]of Object.entries(CELLS))if(cx===x&&cy===y&&(k!=="ul"&&k!=="ur"||dpad.classList.contains("fwd")))return k;return null;}
function applyCell(k){
  if(k===dpCell)return;const prev=dpCell;dpCell=k;
  Object.entries(cellEl).forEach(([n,el])=>el.classList.toggle("on",n===k));
  if(mode==="game"){
    const v={up:[0,1],down:[0,-1],left:[-1,0],right:[1,0],ul:[-.71,.71],ur:[.71,.71]}[k]||[0,0];
    state.lx=v[0];state.ly=v[1];dirty=true;
    dpad.classList.toggle("fwd",k==="up"||k==="ul"||k==="ur");
    if(k==="up"&&prev!=="ul"&&prev!=="ur"){const now=performance.now();if(now-lastUp<320)pulse(B.ls);lastUp=now;}
    if(k==="mid"){sneaking=!sneaking;cellEl.mid.classList.toggle("lock",sneaking);pulse(B.rs);}
  }else{
    ["up","down","left","right"].forEach(d=>setBit(B[d],k===d));
    setBit(B.south,k==="mid");
  }
  if(k)buzz(8);
}
dpad.addEventListener("pointerdown",e=>{e.preventDefault();e.stopPropagation();if(dpId!==null)return;dpId=e.pointerId;dpad.setPointerCapture(dpId);applyCell(cellAt(e));});
dpad.addEventListener("pointermove",e=>{if(e.pointerId!==dpId)return;const k=cellAt(e);if(k!=="mid"||dpCell===null)applyCell(k);});
const dpEnd=e=>{if(e.pointerId!==dpId)return;dpId=null;applyCell(null);state.lx=0;state.ly=0;dirty=true;
  ["up","down","left","right","south"].forEach(d=>setBit(B[d],false));dpad.classList.remove("fwd");};
dpad.addEventListener("pointerup",dpEnd);dpad.addEventListener("pointercancel",dpEnd);

// Pular (nos menus: voltar).
const jump=$("jump");
hold(jump,()=>{jump.classList.add("on");setBit(mode==="game"?B.south:B.east,true);buzz(12);},
  ()=>{jump.classList.remove("on");setBit(B.south,false);setBit(B.east,false);});

// ---- Olhar, usar e quebrar ----------------------------------------------------------
// Onde o dedo encosta aparece o analógico (a bolinha): puxar gira a câmera,
// mais longe do centro gira mais rápido. Toque rápido sem puxar: ataca se a mira
// está num mob, senão usa/coloca. Segurar parado: quebra (e dá para mirar
// puxando a bolinha enquanto quebra).
let sens=+(store("sens")||1.2);
const TAP_MS=480,TAP_PX=18;
// Usar (segurar: comer, arco, escudo; tocar: colocar bloco), sem depender do toque na tela.
const useBtn=$("use");
hold(useBtn,()=>{useBtn.classList.add("on");state.lt=1;dirty=true;buzz(12);},()=>{useBtn.classList.remove("on");state.lt=0;dirty=true;});
const look=$("look"),lstick=$("lstick"),lknob=lstick.firstElementChild,LR=60;let lk=null;
look.addEventListener("pointerdown",e=>{if(lk||mode!=="game"||e.target!==look)return;
  lk={id:e.pointerId,ox:e.clientX,oy:e.clientY,t:performance.now(),moved:false,mining:false};
  look.setPointerCapture(e.pointerId);$("hint").style.display="none";
  lstick.style.left=e.clientX+"px";lstick.style.top=e.clientY+"px";lknob.style.transform="";lstick.style.display="block";
  // Toque até TAP_MS e com o dedo quase parado (TAP_PX) é "usar": um toque meio
  // demorado não vira quebrar. Quebrar só segurando parado além disso.
  lk.timer=setTimeout(()=>{if(lk&&!lk.moved){lk.mining=true;state.rt=1;dirty=true;buzz(15);}},TAP_MS);});
look.addEventListener("pointermove",e=>{if(!lk||e.pointerId!==lk.id)return;
  let dx=e.clientX-lk.ox,dy=e.clientY-lk.oy;const d=Math.hypot(dx,dy);
  if(d>TAP_PX)lk.moved=true;
  if(d>LR){dx*=LR/d;dy*=LR/d;}
  lknob.style.transform=`translate(${dx}px,${dy}px)`;
  // Zona morta pequena e curva: perto do centro mira fino, na borda gira rápido.
  const c=v=>{const a=Math.abs(v);return a<0.08?0:Math.sign(v)*Math.min(1,Math.pow((a-0.08)/0.92,1.5)*sens);};
  state.rx=c(dx/LR);state.ry=c(-dy/LR);dirty=true;});
const lookEnd=e=>{if(!lk||e.pointerId!==lk.id)return;clearTimeout(lk.timer);
  if(lk.mining){state.rt=0;}
  else if(!lk.moved&&performance.now()-lk.t<TAP_MS){if(target==="entity")trig("rt");else trig("lt");buzz(10);}
  lk=null;lstick.style.display="none";state.rx=0;state.ry=0;dirty=true;};
look.addEventListener("pointerup",lookEnd);look.addEventListener("pointercancel",lookEnd);
function lookStep(){}
function release(){Object.assign(state,{lx:0,ly:0,rx:0,ry:0,lt:0,rt:0,btn:0});dirty=true;lk=null;dpId=null;dpCell=null;
  lstick.style.display="none";document.querySelectorAll(".on").forEach(b=>b.classList.remove("on"));dpad.classList.remove("fwd");}

// Ajustes: sensibilidade do olhar.
function sensLabel(){mc($("sensT"),`${tr("sens")}: ${sens.toFixed(1)}`,{s:2});}
$("sens").value=sens;sensLabel();
$("sens").addEventListener("input",()=>{sens=+$("sens").value;store("sens",sens);sensLabel();});
$("cfgBtn").addEventListener("click",()=>{document.body.classList.add("cfg");release();});
$("cfgOk").addEventListener("click",()=>document.body.classList.remove("cfg"));

// ---- Editar o HUD ------------------------------------------------------------
// D-pad, pular, usar, hotbar, botões de cima e o status: cada peça pode ser
// arrastada, aumentada ou ficar mais transparente. Fica guardado no celular
// (translate/scale/opacity por cima do lugar padrão, que "Restaurar" devolve).
const EDIT=["st","top","jump","use","hud","dpad"];
let layout={};try{layout=JSON.parse(store("layout")||"{}")||{};}catch(e){layout={};}
function lay(id){return layout[id]||(layout[id]={x:0,y:0,s:1,o:1});}
function applyLayout(){EDIT.forEach(id=>{const e=$(id),l=layout[id];
  e.style.translate=l?`${l.x}px ${l.y}px`:"";e.style.scale=l?String(l.s):"";e.style.opacity=l&&l.o<1?String(l.o):"";});}
// #top ocupa a largura toda: vale o retângulo dos botões dele.
function rectOf(id){const e=$(id);if(id!=="top")return e.getBoundingClientRect();
  const rs=[...e.children].map(c=>c.getBoundingClientRect());
  return{left:Math.min(...rs.map(r=>r.left)),right:Math.max(...rs.map(r=>r.right)),top:Math.min(...rs.map(r=>r.top)),bottom:Math.max(...rs.map(r=>r.bottom))};}
function clampIn(id){const r=rectOf(id),l=lay(id);
  if(r.left<0)l.x-=r.left;else if(r.right>innerWidth)l.x-=r.right-innerWidth;
  if(r.top<0)l.y-=r.top;else if(r.bottom>innerHeight)l.y-=r.bottom-innerHeight;applyLayout();}
let esel="dpad",edrag=null;
function editSelect(id){esel=id;EDIT.forEach(k=>$(k).classList.toggle("esel",k===id));const l=lay(id);$("eSize").value=l.s;$("eOp").value=l.o;}
function editSave(){store("layout",JSON.stringify(layout));}
$("editBtn").addEventListener("click",()=>{release();document.body.classList.remove("cfg");document.body.classList.add("edit");editSelect(esel);buzz(10);});
$("eDone").addEventListener("click",()=>{document.body.classList.remove("edit");EDIT.forEach(k=>$(k).classList.remove("esel"));editSave();buzz(10);});
$("eReset").addEventListener("click",()=>{layout={};applyLayout();editSelect(esel);editSave();buzz(15);});
$("eSize").addEventListener("input",e=>{lay(esel).s=+e.target.value;applyLayout();clampIn(esel);});
$("eOp").addEventListener("input",e=>{lay(esel).o=+e.target.value;applyLayout();});
const editLayer=$("edit");
editLayer.addEventListener("pointerdown",e=>{if(e.target.closest("#ebar"))return;
  const hit=EDIT.find(id=>{const el=$(id);if(getComputedStyle(el).display==="none")return false;const r=rectOf(id);
    return e.clientX>=r.left-8&&e.clientX<=r.right+8&&e.clientY>=r.top-8&&e.clientY<=r.bottom+8;});
  if(!hit)return;editSelect(hit);const l=lay(hit);edrag={id:e.pointerId,px:e.clientX,py:e.clientY,x:l.x,y:l.y};
  editLayer.setPointerCapture(e.pointerId);buzz(8);});
editLayer.addEventListener("pointermove",e=>{if(!edrag||e.pointerId!==edrag.id)return;const l=lay(esel);
  l.x=Math.round(edrag.x+e.clientX-edrag.px);l.y=Math.round(edrag.y+e.clientY-edrag.py);applyLayout();});
const edEnd=e=>{if(!edrag||e.pointerId!==edrag.id)return;edrag=null;clampIn(esel);};
editLayer.addEventListener("pointerup",edEnd);editLayer.addEventListener("pointercancel",edEnd);
applyLayout();

// ---- Nomes e ícones dos itens --------------------------------------------
let names={};
// ---- Idioma: o mesmo do jogo -------------------------------------------------
// O jogo manda o código dele ("pt_br", "en_us"...) no estado. Os nomes e o que o
// jogo já traduz vêm do arquivo de idioma dele (/lang/<código>.json); o que só
// existe no PocketPad, do dicionário abaixo (inglês para os outros idiomas).
function G(key,own){return names[key]||tr(own);}
function applyLang(){
  document.documentElement.lang=LANG.replace("_","-");
  document.querySelectorAll("[data-tk]").forEach(el=>mc(el,el.dataset.g?G(el.dataset.g,el.dataset.tk):tr(el.dataset.tk)));
  $("chatIn").placeholder=tr("msg");
  document.querySelectorAll("[data-al]").forEach(el=>el.setAttribute("aria-label",tr(el.dataset.al)));
  if(stKey)mc($("stt"),tr(stKey),{s:2});
  if(overKey&&over.style.display!=="none")show(overKey);
  sensLabel();
  lastCarried="";tipKey="";winKey="";tabsKey="";if(lastState)onState(lastState);else updateTip();
}
let langLoading="";
function setLang(code){
  code=String(code||"").toLowerCase();if(!/^[a-z]{2,3}_[a-z0-9]{2,4}$/.test(code)||code===langLoading)return;
  langLoading=code;LANG=code;
  LANGP=fetch(`/lang/${code}.json`).then(r=>r.ok?r:fetch("/lang/en_us.json")).then(r=>r.json())
    .then(j=>{if(langLoading!==code)return;names=j;applyLang();}).catch(()=>applyLang());
  return LANGP;
}
let LANGP=Promise.resolve();
document.querySelectorAll("[data-tk]").forEach(el=>mc(el,tr(el.dataset.tk)));mc($("stt"),tr("connecting"),{s:2});
setTimeout(()=>setLang(LANG),0);
function itemName(id,potion){const n=id.replace(/^minecraft:/,"");
  if(potion){const p=String(potion).replace(/^minecraft:/,"");const e=names[`item.minecraft.${n}.effect.${p}`];if(e)return e;}
  return names["item.minecraft."+n]||names["block.minecraft."+n]||n.replace(/_/g," ");}
const icons=new Map();
function iconInfo(id){if(!icons.has(id))icons.set(id,fetch(`/icon?id=${encodeURIComponent(id)}`).then(r=>r.json()).catch(()=>({k:"none"})));return icons.get(id);}
const TINT=[[/birch_leaves/,"#80a755"],[/spruce_leaves/,"#619961"],[/(cherry|azalea|pale_oak)_leaves/,null],[/(grass_block_top|_leaves|block\/vine|short_grass|tall_grass|fern|lily_pad)/,"#79c05a"]];
function tintOf(t){for(const[re,c]of TINT)if(re.test(t))return c;return null;}
function texCss(t){const c=tintOf(t),u=`url(/tex/${t}.png)`;return c?`background-image:${u};background-color:${c}`:`background-image:${u}`;}
// ---- Modelos 3D dos itens, como o jogo desenha na interface -----------------
// Itens de bloco: items/<id>.json e a cadeia de modelos do jogo (os cuboides
// "elements", com UV e rotação, e o "display.gui" do modelo). Composto (a cama),
// seleção e condição seguem o caminho padrão. Os itens que o jogo desenha por
// código (baú, shulker, estandarte, crânios, escudo, tridente, vaso, conduíte,
// estátua de cobre) usam as camadas de entity-models.json, a mesma geometria
// do vanilla. Itens planos usam as camadas de textura, com o tom do jogo
// (poções com a cor do efeito). Tudo em CSS 3D, uma div por face.
const AJ=new Map();
let BUNDLE=null,BUNDLEP=null;
function loadBundle(){if(!BUNDLEP)BUNDLEP=fetch("/bundle.json").then(r=>r.ok?r.json():null).then(b=>{BUNDLE=b;}).catch(()=>{});return BUNDLEP;}
function aj(p){if(!AJ.has(p))AJ.set(p,BUNDLE?Promise.resolve(BUNDLE[p]??null):fetch("/a/"+p).then(r=>r.ok?r.json():null).catch(()=>null));return AJ.get(p);}
let EM=null;
function entityModels(){if(!EM)EM=fetch("/entity-models.json").then(r=>r.ok?r.json():{}).catch(()=>({}));return EM;}
const TDIM=new Map();
const TDIM_NOW=new Map();
function texDim(t){if(!TDIM.has(t))TDIM.set(t,new Promise(res=>{const i=new Image();i.onload=()=>{const d={w:i.naturalWidth,h:i.naturalHeight,img:i};TDIM_NOW.set(t,d);res(d);};i.onerror=()=>res(null);i.src=`/tex/${t}.png`;}));return TDIM.get(t);}
const nsx=r=>String(r||"").replace(/^minecraft:/,"");
const hex6=n=>"#"+((n>>>0)&0xFFFFFF).toString(16).padStart(6,"0");
const DYE={white:"#f9fffe",orange:"#f9801d",magenta:"#c74ebd",light_blue:"#3ab3da",yellow:"#fed83d",lime:"#80c71f",pink:"#f38baa",gray:"#474f52",light_gray:"#9d9d97",cyan:"#169c9c",purple:"#8932b8",blue:"#3c44aa",brown:"#835432",green:"#5e7c16",red:"#b02e26",black:"#1d1d21"};

// ---- Matrizes 4 × 4 (colunas), como glam/JOML --------------------------------
const M4={
  I:()=>[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1],
  mul(a,b){const o=new Array(16);for(let c=0;c<4;c++)for(let r=0;r<4;r++){let s=0;for(let k=0;k<4;k++)s+=a[k*4+r]*b[c*4+k];o[c*4+r]=s;}return o;},
  T:(x,y,z)=>[1,0,0,0,0,1,0,0,0,0,1,0,x,y,z,1],
  S:(x,y,z)=>[x,0,0,0,0,y,0,0,0,0,z,0,0,0,0,1],
  Q([x,y,z,w]){return[1-2*(y*y+z*z),2*(x*y+z*w),2*(x*z-y*w),0, 2*(x*y-z*w),1-2*(x*x+z*z),2*(y*z+x*w),0, 2*(x*z+y*w),2*(y*z-x*w),1-2*(x*x+y*y),0, 0,0,0,1];},
  RX:a=>{const c=Math.cos(a),s=Math.sin(a);return[1,0,0,0,0,c,s,0,0,-s,c,0,0,0,0,1];},
  RY:a=>{const c=Math.cos(a),s=Math.sin(a);return[c,0,-s,0,0,1,0,0,s,0,c,0,0,0,0,1];},
  RZ:a=>{const c=Math.cos(a),s=Math.sin(a);return[c,s,0,0,-s,c,0,0,0,0,1,0,0,0,0,1];},
  ap:(m,[x,y,z])=>[m[0]*x+m[4]*y+m[8]*z+m[12],m[1]*x+m[5]*y+m[9]*z+m[13],m[2]*x+m[6]*y+m[10]*z+m[14]],
};
// `Transformation` de um items/*.json: translação (blocos), rotações e escala.
function itemTransform(tr){
  if(!tr)return M4.I();
  const t=tr.translation||[0,0,0],s=tr.scale||[1,1,1];
  return M4.mul(M4.mul(M4.mul(M4.T(t[0]*16,t[1]*16,t[2]*16),M4.Q(tr.left_rotation||[0,0,0,1])),M4.S(s[0],s[1],s[2])),M4.Q(tr.right_rotation||[0,0,0,1]));
}

// ---- Modelos de bloco/item ------------------------------------------------------
async function resolveModel(ref){
  const textures={};let elements=null,gui=null,generated=false,cur=nsx(ref);
  for(let i=0;i<12&&cur;i++){
    if(/^(builtin\/generated|item\/generated|item\/handheld)/.test(cur))generated=true;
    const m=await aj(`models/${cur}.json`);if(!m)break;
    // Uma textura pode vir como texto ou como objeto ({"sprite": ..., "force_translucent": ...}).
    for(const[k,v]of Object.entries(m.textures||{}))if(!(k in textures))textures[k]=typeof v==="string"?v:(v&&(v.sprite||v.texture))||null;
    if(!elements&&m.elements)elements=m.elements;
    if(!gui&&m.display&&m.display.gui)gui=m.display.gui;
    cur=m.parent?nsx(m.parent):null;
  }
  const res=k=>{let v=textures[k];for(let i=0;i<8&&v&&v.startsWith("#");i++)v=textures[v.slice(1)];return v?nsx(v):null;};
  return{elements,gui,generated:generated&&!elements,res};
}
// O caminho padrão de um items/<id>.json: modelos, deslocamentos e tons.
// `tf`: as transformações dos nós de cima (select, condition...), que valem
// para tudo abaixo deles (a estátua de cobre vira de cabeça para baixo assim).
function itemParts(m,off,tf){
  if(!m||typeof m!=="object")return[];
  const t=nsx(m.type);
  if(t!=="composite"&&m.transformation&&t!=="special")tf=M4.mul(tf||M4.I(),itemTransform(m.transformation));
  const add=(o,tr)=>{const p=tr&&tr.translation;return p?[o[0]+p[0],o[1]+p[1],o[2]+p[2]]:o;};
  if(t==="model")return[{ref:m.model,off,tints:m.tints||[]}];
  if(t==="composite")return(m.models||[]).flatMap(x=>itemParts(x,add(off,x.transformation),tf));
  if(t==="condition")return itemParts(m.on_false,off,tf);
  if(t==="select"){
    // display_context: o caso da interface ("gui"), como o tridente plano no inventário.
    if(nsx(m.property)==="display_context"){const c=(m.cases||[]).find(c=>[].concat(c.when).includes("gui"));if(c)return itemParts(c.model,off,tf);}
    return itemParts(m.fallback||(m.cases&&m.cases[0]&&m.cases[0].model),off,tf);}
  if(t==="range_dispatch")return itemParts(m.fallback||(m.entries&&m.entries[0]&&m.entries[0].model),off,tf);
  if(t==="special")return[{special:m.model||{},base:m.base,tf:M4.mul(tf||M4.I(),itemTransform(m.transformation)),off}];
  return[];
}
// O tom de uma camada (`tints` do item): poção com a cor do efeito; os outros, o padrão.
function tintColor(tint,meta){
  if(!tint)return null;const t=nsx(tint.type);
  if(t==="potion")return meta&&meta.color?"#"+meta.color:hex6(tint.default??-13083194);
  if(t==="constant")return hex6(tint.value);
  if(tint.default!==undefined)return hex6(tint.default);
  return null;
}

// ---- Os especiais: camada do entity-models, textura, pose e cor -----------------
function specialSpec(sp){
  const t=nsx(sp.type),tex=nsx(sp.texture);
  const ent=d=>"entity/"+d+"/"+(tex||"");
  switch(t){
    case"chest":return[{layer:"minecraft:chest#main",tex:"entity/chest/"+(tex||"normal")}];
    case"shulker_box":return[{layer:"minecraft:shulker_box#main",tex:"entity/shulker/"+(tex||"shulker")}];
    case"head":{const k=nsx(sp.kind);const L={skeleton:"skeleton_skull",wither_skeleton:"wither_skeleton_skull",zombie:"zombie_head",creeper:"creeper_head",dragon:"dragon_skull",piglin:"piglin_head"}[k]||"player_head";
      const T={skeleton:"entity/skeleton/skeleton",wither_skeleton:"entity/skeleton/wither_skeleton",zombie:"entity/zombie/zombie",creeper:"entity/creeper/creeper",dragon:"entity/enderdragon/dragon",piglin:"entity/piglin/piglin"}[k]||"entity/player/wide/steve";
      return[{layer:`minecraft:${L}#main`,tex:T,pose:{head:p=>{p[3]=0;p[4]=0;}}}];}
    case"player_head":return[{layer:"minecraft:player_head#main",tex:"entity/player/wide/steve"}];
    case"banner":{const flag={flag:p=>{p[3]=-0.0025*Math.PI;}};const c=DYE[nsx(sp.color)]||DYE.white;
      return[{layer:"minecraft:standing_banner#main",tex:"entity/banner/banner_base"},{layer:"minecraft:standing_banner#flag",tex:"entity/banner/base",color:c,pose:flag}];}
    case"shield":return[{layer:"minecraft:shield#main",tex:"entity/shield/shield_base_nopattern"}];
    case"trident":return[{layer:"minecraft:trident#main",tex:"entity/trident/trident",alt:"entity/trident"}];
    case"conduit":return[{layer:"minecraft:conduit#shell",tex:"entity/conduit/base"}];
    case"decorated_pot":return[{layer:"minecraft:decorated_pot_base#main",tex:"entity/decorated_pot/decorated_pot_base"},{layer:"minecraft:decorated_pot_sides#main",tex:"entity/decorated_pot/decorated_pot_side"}];
    case"copper_golem_statue":{const L={running:"copper_golem_running",sitting:"copper_golem_sitting",star:"copper_golem_star"}[nsx(sp.pose)]||"copper_golem";
      return[{layer:`minecraft:${L}#main`,tex:nsx(String(sp.texture||"entity/copper_golem/copper_golem")).replace(/^textures\//,"").replace(/\.png$/,""),pose:{root:p=>{p[1]=0;p[5]=Math.PI;}}}];}
    case"bell":return[{layer:"minecraft:bell#main",tex:"entity/bell/bell_body"}];
  }
  return null;
}
// As faces de uma camada: cada polígono [x,y,z,u,v] × 4, pela pose de cada parte
// (`translateAndRotate`: translação/16, rotação ZYX, escala).
function layerQuads(part,name,parent,pose,out){
  const p=part.pose.slice();if(pose&&pose[name])pose[name](p);
  const m=M4.mul(M4.mul(M4.mul(parent,M4.T(p[0],p[1],p[2])),M4.mul(M4.mul(M4.RZ(p[5]),M4.RY(p[4])),M4.RX(p[3]))),M4.S(p[6],p[7],p[8]));
  for(const poly of part.polygons||[])out.push(poly.map(v=>[...M4.ap(m,[v[0],v[1],v[2]]),v[3],v[4]]));
  for(const[n,c]of Object.entries(part.children||{}))layerQuads(c,n,m,pose,out);
}

const MDESC=new Map();
function modelDesc(id){
  if(!MDESC.has(id))MDESC.set(id,(async()=>{
    const name=nsx(id);const it=await aj(`items/${name}.json`);if(!it)return null;
    const parts=itemParts(it.model,[0,0,0]);if(!parts.length)return null;
    const out=[];let gui=null;
    for(const p of parts){
      if(p.special){
        const spec=specialSpec(p.special);if(!spec)return null;
        const em=await entityModels();
        const base=p.base?await resolveModel(p.base):null;gui=gui||(base&&base.gui);
        const tm=p.tf;
        for(const s of spec){const layer=em[s.layer];if(!layer)continue;
          const quads=[];layerQuads(layer,"root",tm,s.pose,quads);
          let tex=s.tex;if(s.alt&&!(await texDim(tex)))tex=s.alt;
          out.push({quads,tex,color:s.color});}
        continue;
      }
      const m=await resolveModel(p.ref);
      if(m.generated&&parts.length===1){
        const layers=[0,1,2,3].map(i=>({t:m.res("layer"+i),tint:p.tints[i]})).filter(l=>l.t);
        return layers.length?{kind:"flat",layers}:null;
      }
      if(!m.elements)return null;
      gui=gui||m.gui;out.push({elements:m.elements,res:m.res,off:p.off});
    }
    if(!out.length)return null;
    return{kind:"3d",parts:out,gui:gui||{rotation:[30,225,0],translation:[0,0,0],scale:[0.625,0.625,0.625]}};
  })());
  return MDESC.get(id);
}

// ---- Desenho em CSS 3D ------------------------------------------------------------
const SHADE={up:1,down:.5,north:.8,south:.8,east:.6,west:.6};
function defUV(f,a,b){const[x1,y1,z1]=a,[x2,y2,z2]=b;return{down:[x1,16-z2,x2,16-z1],up:[x1,z1,x2,z2],north:[16-x2,16-y2,16-x1,16-y1],south:[x1,16-y2,x2,16-y1],west:[z1,16-y2,z2,16-y1],east:[16-z2,16-y2,16-z1,16-y1]}[f];}
const cross=(a,b)=>[a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0]];
const sub=(a,b)=>[a[0]-b[0],a[1]-b[1],a[2]-b[2]];
// Brilho de uma face pela normal (luz de interface: de cima e da frente).
function shadeOf(n){const l=Math.hypot(...n)||1,y=-n[1]/l,z=n[2]/l,x=n[0]/l;return Math.max(.45,Math.min(1,.6+.4*y+.15*z-.05*Math.abs(x)));}
// Uma face: uma div colocada pela matriz (U, V em px da div, N) com a região da textura.
function faceDiv(P,U,V,W,H,css,shade){
  const N=cross(U,V),nl=Math.hypot(...N)||1;
  const d=document.createElement("div");d.className="mf";
  d.style.cssText=`width:${W}px;height:${H}px;transform:matrix3d(${U[0]/W},${U[1]/W},${U[2]/W},0,${V[0]/H},${V[1]/H},${V[2]/H},0,${N[0]/nl},${N[1]/nl},${N[2]/nl},0,${P[0]},${P[1]},${P[2]},1);filter:brightness(${shade});${css}`;
  return d;
}
// O modelo desenhado num canvas: a vista da interface é ortográfica, então cada
// face é um paralelogramo na tela e a textura vai por uma transformação afim
// (drawImage com setTransform), da face mais funda para a mais próxima. As faces
// dos modelos de bloco somem de costas (como o CSS fazia); as das camadas de
// entidade não (o jogo desenha as duas, `cull_back: false`) e a ordem pela
// profundidade esconde as de trás. Cada item fica pronto uma vez por tamanho:
// rolar o criativo só copia a imagem.
const RCACHE=new Map();
const CSSR={
  x:(a,[x,y,z])=>{const c=Math.cos(a),s=Math.sin(a);return[x,y*c-z*s,y*s+z*c];},
  y:(a,[x,y,z])=>{const c=Math.cos(a),s=Math.sin(a);return[x*c+z*s,y,-x*s+z*c];},
  z:(a,[x,y,z])=>{const c=Math.cos(a),s=Math.sin(a);return[x*c-y*s,x*s+y*c,z];},
};
const RAD=Math.PI/180;
function placeCanvas(ico,src,L){
  const c=document.createElement("canvas");c.width=src.width;c.height=src.height;
  c.style.cssText=`position:absolute;left:50%;top:50%;width:${L}px;height:${L}px;margin:${-L/2}px 0 0 ${-L/2}px;image-rendering:pixelated`;
  c.getContext("2d").drawImage(src,0,0);ico.innerHTML="";ico.appendChild(c);
}
async function render3D(ico,d,size,key){
  const L=Math.ceil(size*1.5),ck=key?key+"|"+size+"|"+DPR:null;
  if(ck&&RCACHE.has(ck)){const done=await RCACHE.get(ck);if(done&&ico.isConnected)placeCanvas(ico,done,L);return;}
  const job=drawModel(d,size,L);if(ck)RCACHE.set(ck,job);
  const done=await job;if(done&&ico.isConnected)placeCanvas(ico,done,L);
}
async function drawModel(d,size,L){
  const k=size*(16/18)/16;      // px por unidade do modelo (o item ocupa 16 de 18)
  const g=d.gui,r=g.rotation||[0,0,0],t=g.translation||[0,0,0],s=g.scale||[1,1,1];
  // A transformação "gui" do modelo, na ordem do CSS de antes:
  // translate · rotateX(-r0) · rotateY(r1) · rotateZ(-r2) · scale.
  const root=p=>{let q=[p[0]*s[0],p[1]*s[1],p[2]*s[2]];q=CSSR.z(-r[2]*RAD,q);q=CSSR.y(r[1]*RAD,q);q=CSSR.x(-r[0]*RAD,q);return[q[0]+t[0]*k,q[1]-t[1]*k,q[2]+t[2]*k];};
  const C=(x,y,z,o=[0,0,0])=>[(x+o[0]*16-8)*k,-(y+o[1]*16-8)*k,(z+o[2]*16-8)*k];
  const add=(a,b)=>[a[0]+b[0],a[1]+b[1],a[2]+b[2]],mulv=(a,f)=>[a[0]*f,a[1]*f,a[2]*f];
  // Cada face: textura, retângulo na textura (px) e onde caem três cantos dele
  // (A = canto de cima à esquerda, B = de cima à direita, C = de baixo à esquerda).
  const faces=[];
  const texs=new Set();
  for(const part of d.parts){if(part.quads)texs.add(part.tex);else for(const el of part.elements)for(const f of Object.values(el.faces||{})){const x=part.res(String(f.texture||"").replace(/^#/,""));if(x)texs.add(x);}}
  await Promise.all([...texs].map(texDim));
  for(const part of d.parts){
    if(part.quads){
      const dim=TDIM_NOW.get(part.tex);if(!dim)continue;
      for(const q of part.quads){
        const us=q.map(v=>v[3]),vs=q.map(v=>v[4]),u0=Math.min(...us),u1=Math.max(...us),v0=Math.min(...vs),v1=Math.max(...vs);
        if(u1-u0<1e-6||v1-v0<1e-6)continue;
        const at=(u,v)=>q.find(p=>Math.abs(p[3]-u)<1e-6&&Math.abs(p[4]-v)<1e-6);
        const o=at(u0,v0),x=at(u1,v0),y=at(u0,v1);if(!o||!x||!y)continue;
        const A=C(o[0],o[1],o[2]),B=C(x[0],x[1],x[2]),Cc=C(y[0],y[1],y[2]);
        faces.push({tex:part.tex,img:dim.img,src:[u0*dim.w,v0*dim.h,(u1-u0)*dim.w,(v1-v0)*dim.h],A,B,C:Cc,cull:false,
          shade:shadeOf(cross(sub(B,A),sub(Cc,A))),color:part.color||null,rot:null});
      }
      continue;
    }
    for(const el of part.elements){
      const a=el.from,b=el.to,o=part.off;
      let rot=null;
      if(el.rotation&&el.rotation.angle){const ro=el.rotation.origin||[8,8,8],pv=C(ro[0],ro[1],ro[2],o),an=el.rotation.angle*RAD,ax=el.rotation.axis;
        const f=ax==="x"?q=>CSSR.x(-an,q):ax==="y"?q=>CSSR.y(an,q):q=>CSSR.z(-an,q);
        rot=p=>add(f(sub(p,pv)),pv);}
      const F={
        north:{P:C(b[0],b[1],a[2],o),U:[-1,0,0],V:[0,1,0],w:b[0]-a[0],h:b[1]-a[1]},
        south:{P:C(a[0],b[1],b[2],o),U:[1,0,0],V:[0,1,0],w:b[0]-a[0],h:b[1]-a[1]},
        east:{P:C(b[0],b[1],b[2],o),U:[0,0,-1],V:[0,1,0],w:b[2]-a[2],h:b[1]-a[1]},
        west:{P:C(a[0],b[1],a[2],o),U:[0,0,1],V:[0,1,0],w:b[2]-a[2],h:b[1]-a[1]},
        up:{P:C(a[0],b[1],a[2],o),U:[1,0,0],V:[0,0,1],w:b[0]-a[0],h:b[2]-a[2]},
        down:{P:C(a[0],a[1],b[2],o),U:[1,0,0],V:[0,0,-1],w:b[0]-a[0],h:b[2]-a[2]},
      };
      for(const[fn,face]of Object.entries(el.faces||{})){
        const geo=F[fn];if(!geo||geo.w<=0||geo.h<=0)continue;
        const tex=part.res(String(face.texture||"").replace(/^#/,""));if(!tex)continue;
        const dim=TDIM_NOW.get(tex);if(!dim)continue;
        const W=geo.w*k,H=geo.h*k,tl=geo.P,tr=add(tl,mulv(geo.U,W)),bl=add(tl,mulv(geo.V,H)),br=add(tr,mulv(geo.V,H));
        // Cantos da região UV (TL, TR, BR, BL) e onde cada um cai na face, com a
        // rotação da face (a textura gira no sentido horário) e o espelho da UV.
        const uv=face.uv||defUV(fn,a,b);if(Math.abs(uv[2]-uv[0])<1e-6||Math.abs(uv[3]-uv[1])<1e-6)continue;
        const kq=(((face.rotation||0)%360+360)%360)/90|0,fc=[tl,tr,br,bl],at=j=>fc[(j+kq)%4];
        const Fa=at(0),Fb=at(1),Fd=at(3);
        const pt=(u,v)=>add(add(Fa,mulv(sub(Fb,Fa),(u-uv[0])/(uv[2]-uv[0]))),mulv(sub(Fd,Fa),(v-uv[1])/(uv[3]-uv[1])));
        const u0=Math.min(uv[0],uv[2]),u1=Math.max(uv[0],uv[2]),v0=Math.min(uv[1],uv[3]),v1=Math.max(uv[1],uv[3]),per=dim.w/16;
        let A=pt(u0,v0),B=pt(u1,v0),Cc=pt(u0,v1);
        let T1=tl,T2=tr,T3=bl;
        if(rot){A=rot(A);B=rot(B);Cc=rot(Cc);T1=rot(T1);T2=rot(T2);T3=rot(T3);}
        faces.push({tex,img:dim.img,src:[u0*per,v0*per,(u1-u0)*per,(v1-v0)*per],A,B,C:Cc,cull:[T1,T2,T3],
          shade:SHADE[fn],color:face.tintindex!==undefined?tintOf(tex):null});
      }
    }
  }
  // Para a tela: a transformação gui, a ordem pela profundidade e o desenho.
  const cv=document.createElement("canvas");cv.width=cv.height=Math.ceil(L*DPR);
  const g2=cv.getContext("2d");g2.imageSmoothingEnabled=false;
  const half=L/2,list=[];
  for(const f of faces){
    const A=root(f.A),B=root(f.B),Cc=root(f.C);
    if(f.cull){const[p,q,w]=f.cull.map(root),n=cross(sub(q,p),sub(w,p));if(n[2]<=1e-9)continue;}
    list.push({f,A,B,C:Cc,z:(B[2]+Cc[2])/2});
  }
  list.sort((x,y)=>x.z-y.z);
  for(const{f,A,B,C:Cc}of list){
    const[sx,sy,sw,sh]=f.src,tw=Math.max(1,Math.round(sw)),th=Math.max(1,Math.round(sh));
    const tmp=document.createElement("canvas");tmp.width=tw;tmp.height=th;const tc=tmp.getContext("2d");tc.imageSmoothingEnabled=false;
    tc.drawImage(f.img,sx,sy,sw,sh,0,0,tw,th);
    if(f.color){tc.globalCompositeOperation="multiply";tc.fillStyle=f.color;tc.fillRect(0,0,tw,th);
      tc.globalCompositeOperation="destination-in";tc.drawImage(f.img,sx,sy,sw,sh,0,0,tw,th);}
    if(f.shade<1){tc.globalCompositeOperation="source-atop";tc.fillStyle=`rgba(0,0,0,${1-f.shade})`;tc.fillRect(0,0,tw,th);}
    // Um pouco maior (0,4 px de cada lado) para não abrir frestas entre as faces.
    const eu=sub(B,A),ev=sub(Cc,A),lu=Math.hypot(eu[0],eu[1])||1,lv=Math.hypot(ev[0],ev[1])||1,gu=0.4/lu,gv=0.4/lv;
    const ox=A[0]-eu[0]*gu-ev[0]*gv,oy=A[1]-eu[1]*gu-ev[1]*gv;
    g2.setTransform(DPR*eu[0]*(1+2*gu)/tw,DPR*eu[1]*(1+2*gu)/tw,DPR*ev[0]*(1+2*gv)/th,DPR*ev[1]*(1+2*gv)/th,DPR*(half+ox),DPR*(half+oy));
    g2.drawImage(tmp,0,0);
  }
  return cv;
}
// Item plano: as camadas de textura, cada uma com o tom dela (máscara + multiplicar).
function renderFlat(ico,d,meta){
  ico.innerHTML="";
  for(const l of d.layers){
    const c=tintColor(l.tint,meta),u=`url(/tex/${l.t}.png)`;
    const e=document.createElement("div");e.className="flat";
    e.style.cssText=c?`background-image:${u};background-color:${c};background-blend-mode:multiply;-webkit-mask-image:${u};mask-image:${u};-webkit-mask-size:100% auto;mask-size:100% auto;-webkit-mask-repeat:no-repeat;mask-repeat:no-repeat`:`background-image:${u}`;
    ico.appendChild(e);
  }
}

// ---- O jogador, com a skin dele, no quadro do inventário --------------------------
// O jogo manda a skin que desenha ("skins/<sha1>", do cache, ou uma padrão) e se o
// modelo é o de braço fino; troca de skin redesenha os quadros abertos.
let SKIN={tex:"entity/player/wide/steve",slim:false};
function setSkin(tex,slim){tex=tex||"entity/player/wide/steve";slim=!!slim;if(tex===SKIN.tex&&slim===SKIN.slim)return;
  SKIN={tex,slim};document.querySelectorAll(".pbox").forEach(b=>renderPlayer(b,b.clientWidth,b.clientHeight));}
// A camada "player#main" do jogo, virada como uma entidade (y para baixo) e
// encaixada no quadro, como o inventário do PC desenha o personagem.
async function renderPlayer(box,w,h){
  const em=await entityModels(),sk=SKIN,layer=em[sk.slim?"minecraft:player_slim#main":"minecraft:player#main"]||em["minecraft:player#main"];if(!layer||!box.isConnected)return;
  const quads=[];layerQuads(layer,"root",M4.mul(M4.RY(Math.PI*0.88),M4.S(-1,-1,1)),null,quads);
  let lo=[1e9,1e9,1e9],hi=[-1e9,-1e9,-1e9];
  for(const q of quads)for(const v of q)for(let i=0;i<3;i++){lo[i]=Math.min(lo[i],v[i]);hi[i]=Math.max(hi[i],v[i]);}
  const tall=hi[1]-lo[1]||32,unit=(h*0.92)/tall;
  // Leva o modelo para o espaço 0..16 centrado em 8, para o render3D.
  const fit=quads.map(q=>q.map(v=>[(v[0]-(lo[0]+hi[0])/2)+8,(v[1]-(lo[1]+hi[1])/2)+8,(v[2]-(lo[2]+hi[2])/2)+8,v[3],v[4]]));
  const ico=document.createElement("div");ico.className="ico";ico.style.inset="0";box.innerHTML="";box.appendChild(ico);
  await render3D(ico,{kind:"3d",parts:[{quads:fit,tex:sk.tex}],gui:{rotation:[0,0,0],translation:[0,0,0],scale:[1,1,1]}},unit*18);
}

function drawIcon(box,id,size,meta){
  const ico=document.createElement("div");ico.className="ico";box.appendChild(ico);
  // Primeiro o modelo do jogo (3D ou camadas planas com tom); se não houver,
  // o ícone simples do mapa (/icon).
  modelDesc(id).then(d=>{
    if(!ico.isConnected)return;
    if(d&&d.kind==="3d")return render3D(ico,d,size,id);
    if(d&&d.kind==="flat")return renderFlat(ico,d,meta);
    return iconInfo(id).then(info=>{
      if(!ico.isConnected)return;
      if(info.k==="flat"){ico.innerHTML=`<div class="flat${tintOf(info.t)?" tint":""}" style="${texCss(info.t)}"></div>`;}
      else if(info.k==="cube"){const s=Math.round(size*0.89/1.6),h=s/2;
        ico.innerHTML=`<div class="cube" style="width:${s}px;height:${s}px;margin:${-h}px 0 0 ${-h}px;transform:rotateX(-30deg) rotateY(45deg)">`+
          `<b class="${tintOf(info.t)?"tint":""}" style="${texCss(info.t)};transform:rotateX(90deg) translateZ(${h}px)"></b>`+
          `<b style="${texCss(info.f)};transform:translateZ(${h}px);filter:brightness(.8)"></b>`+
          `<b style="${texCss(info.s)};transform:rotateY(-90deg) translateZ(${h}px);filter:brightness(.62)"></b></div>`;}
      else{const t=document.createElement("div");t.className="txt";ico.appendChild(t);mc(t,itemName(id).slice(0,8),{s:1});}
    });
  }).catch(()=>{});
}
function fillSlot(box,st,size){
  box.querySelectorAll(".ico,.cnt").forEach(e=>e.remove());
  if(!st)return;
  drawIcon(box,st[0],size,{color:st[3],potion:st[4]});
  if(st[1]>1){const c=document.createElement("div");c.className="cnt";box.appendChild(c);mc(c,st[1],{s:size<32?1:2});}
}

// ---- Hotbar e vida ---------------------------------------------------------
// Toque seleciona; segurar solta o item (um a cada 0,3 s, como o Bedrock).
const hotbar=$("hotbar"),hs=[];
for(let i=0;i<9;i++){const s=document.createElement("div");s.className="hs";hotbar.appendChild(s);hs.push(s);
  let rep=null;
  hold(s,()=>{send("h "+i);hs.forEach((x,j)=>x.classList.toggle("sel",j===i));buzz(8);
      rep=setTimeout(function drop(){pulse(B.east,80);buzz(20);rep=setTimeout(drop,300);},450);},
    ()=>{clearTimeout(rep);rep=null;});}
hold($("more"),()=>{$("more").classList.add("on");send("e");buzz(12);},()=>$("more").classList.remove("on"));
const HEART={full:[".rr.rr..","rRRrRRr.","rRRRRRr.","rRRRRRr.",".rRRRr..","..rRr...","...r....","........"],
  half:[".rr.kk..","rRRkkkk.","rRRkkkk.","rRRkkkk.",".rRkkk..","..rkk...","...k....","........"],
  empty:[".kk.kk..","kkkkkkk.","kkkkkkk.","kkkkkkk.",".kkkkk..","..kkk...","...k....","........"]};
const HP={r:"#000",R:"#e0242b",k:"#2a2a2a"};
function heart(kind){let r="";HEART[kind].forEach((row,y)=>[...row].forEach((ch,x)=>{if(ch!==".")r+=`<rect x="${x}" y="${y}" width="1" height="1" fill="${HP[ch]}"/>`}));return`<svg viewBox="0 0 8 8">${r}</svg>`;}
let lastHp=null;
function hearts(hp){if(hp===lastHp)return;lastHp=hp;const h=$("hearts");if(hp==null){h.innerHTML="";return;}
  let out="";for(let i=0;i<10;i++){const v=hp-i*2;out+=heart(v>=2?"full":v>=1?"half":"empty");}h.innerHTML=out;}

// ---- Inventário (a janela aberta no PC) -------------------------------------
let shiftMode=false,halfMode=false;
$("modeShift").addEventListener("click",()=>{shiftMode=!shiftMode;$("modeShift").classList.toggle("lock",shiftMode);if(shiftMode){halfMode=false;$("modeHalf").classList.remove("lock");}buzz(10);});
$("modeHalf").addEventListener("click",()=>{halfMode=!halfMode;$("modeHalf").classList.toggle("lock",halfMode);if(halfMode){shiftMode=false;$("modeShift").classList.remove("lock");}buzz(10);});
$("invClose").addEventListener("click",()=>{send("q");buzz(12);});
$("tabL").addEventListener("click",()=>{send("b 0");buzz(8);});
$("tabR").addEventListener("click",()=>{send("b 1");buzz(8);});
// Criativo: a lixeira (botão ao lado ou o slot do jogo) e arrastar a grade para rolar.
$("trash").addEventListener("click",()=>{send("d");buzz(20);});
let swipe=null,swiped=false;
$("win").addEventListener("pointerdown",e=>{swiped=false;swipe=document.body.classList.contains("cre")?{id:e.pointerId,y:e.clientY}:null;});
$("win").addEventListener("pointermove",e=>{if(dragging||!swipe||e.pointerId!==swipe.id)return;const dy=e.clientY-swipe.y,step=Math.max(24,18*winScale);
  if(Math.abs(dy)>=step){scrollRows(dy>0?1:-1);swipe.y=e.clientY;swiped=true;buzz(5);}});
$("win").addEventListener("pointerup",()=>{swipe=null;});
// O título de cada estação: a chave no idioma do jogo e o texto em inglês.
const STATIONS={Crafting:["container.crafting","Crafting"],Furnace:["block.minecraft.furnace","Furnace"],Smoker:["block.minecraft.smoker","Smoker"],BlastFurnace:["block.minecraft.blast_furnace","Blast Furnace"],Anvil:["block.minecraft.anvil","Anvil"],Enchanting:["block.minecraft.enchanting_table","Enchanting Table"],Stonecutter:["block.minecraft.stonecutter","Stonecutter"],Loom:["block.minecraft.loom","Loom"],Grindstone:["block.minecraft.grindstone","Grindstone"],Smithing:["block.minecraft.smithing_table","Smithing Table"],CartographyTable:["block.minecraft.cartography_table","Cartography Table"]};
function stationName(st){const e=STATIONS[st];return e?(names[e[0]]||e[1]):st;}
const GUI="/tex/gui/";
function L(key,fb){return names[key]||fb;}

// ---- Catálogo do criativo em cache --------------------------------------------
// O jogo manda as abas e os itens de cada uma (com uma versão) e o mapa de
// ícones de todos eles. O celular guarda os dois e navega sozinho: trocar de
// aba, rolar e buscar não perguntam nada ao jogo. Só pegar um item vai ao jogo.
// Um item fora do mapa (de mod, por exemplo) ainda busca o ícone na hora.
const CAT={v:"",tabs:null,loading:""};
let ctab=null,cfirst={},cquery="";
function loadCatalog(v){
  if(!v||CAT.v===v)return Promise.resolve();if(CAT.loading===v)return CAT.p;
  CAT.p=loadCatalogNow(v);return CAT.p;
}
async function loadCatalogNow(v){
  CAT.loading=v;
  try{
    let cat=null,ico=null;
    try{const c=localStorage.getItem("cc.cat");if(c){const j=JSON.parse(c);if(j.v===v){cat=j.cat;ico=j.ico;}}}catch(e){}
    if(!cat){const [a,b]=await Promise.all([fetch("/creative.json").then(r=>r.json()),fetch("/icons.json").then(r=>r.json())]);
      cat=a;ico=b.icons||{};
      try{localStorage.setItem("cc.cat",JSON.stringify({v,cat,ico}));}catch(e){}}
    for(const[id,info]of Object.entries(ico))if(!icons.has(id))icons.set(id,Promise.resolve(info));
    CAT.tabs=cat.tabs;CAT.v=v;
    if(lastState&&lastState.win&&lastState.win.cr)onState(lastState);
  }catch(e){}finally{if(CAT.loading===v)CAT.loading="";}
}
function catTab(i){return CAT.tabs&&CAT.tabs.find(t=>t[0]===i);}
// A lista da aba local, com a posição original de cada item (a busca filtra pelo nome).
function catList(t){
  const all=t[6].map((id,i)=>[id,i]);
  if(t[5]!=="search"||!cquery)return all;
  // A busca do jogo (a mesma do PC); enquanto a resposta não chega, o resultado
  // anterior; sem resposta (demo, jogo antigo), o filtro pelo nome.
  if(SQ.set&&(SQ.setQ===cquery||SQ.waiting))return all.filter(([,i])=>SQ.set.has(i));
  const q=cquery.toLowerCase();
  return all.filter(([raw])=>{const[id,,p]=raw.split("|");return itemName(id,p).toLowerCase().includes(q)||id.includes(q);});
}
function chooseTab(i){
  const t=catTab(i);if(!t)return;
  ctab=i;pendingTab=i;send("ct "+i);buzz(8);
  setTimeout(()=>{if(pendingTab===i)pendingTab=null;},1500);showTip(null,t[2]);
  if(t[5]!=="search")setSearch(false);
  if(lastState)onState(lastState);
}
function scrollRows(n){
  const t=CAT.tabs&&catTab(ctab);
  if(t&&t[5]!=="inv"){const rows=Math.ceil(catList(t).length/9),max=Math.max(0,rows-5);
    const f=Math.max(0,Math.min(max,(cfirst[ctab]||0)-n));if(f!==(cfirst[ctab]||0)){cfirst[ctab]=f;if(lastState)onState(lastState);}return;}
  send(`w ${n}`);
}
// A janela do criativo montada do cache: grade 9 × 5 em (9 + 18c, 18 + 18l),
// hotbar do jogo em y = 112, abas e barra como no PC.
// A aba vale nos dois lados: o celular pede a sua ("ct"), e se o PC troca de
// aba por conta própria (mouse, teclado), o celular segue o PC.
let gameSel=null,pendingTab=null;
function localCreative(sw){
  if(ctab===null||!catTab(ctab))ctab=sw.cr.sel;
  if(sw.cr.sel===pendingTab)pendingTab=null;
  else if(pendingTab===null&&gameSel!==null&&sw.cr.sel!==gameSel&&catTab(sw.cr.sel)){ctab=sw.cr.sel;if(catTab(ctab)[5]!=="search")setSearch(false);}
  gameSel=sw.cr.sel;
  const t=catTab(ctab);
  const tabs=CAT.tabs.map(x=>[x[0],x[1],x[2],x[3],x[4],x[5]]);
  if(t[5]==="inv"){
    // A aba de inventário é do jogo: mostra a dele quando o PC chegar nela.
    return{...sw,cr:{...sw.cr,tabs,sel:ctab}};
  }
  const list=catList(t),rows=Math.ceil(list.length/9),first=Math.min(cfirst[ctab]||0,Math.max(0,rows-5));
  const slots=[];
  const st=raw=>{const[id,c,p]=raw.split("|");return c?[id,1,0,c,p]:[id,1,0];};
  for(let k=0;k<45;k++){const e=list[first*9+k];slots.push([900+k,9+(k%9)*18,18+Math.floor(k/9)*18,e?st(e[0]):null]);}
  for(let i=0;i<9;i++){const s=sw.slots.find(x=>x[0]===i);slots.push([i,9+i*18,112,s?s[3]:(lastState.hb||[])[i]||null]);}
  const map={};for(let k=0;k<45;k++){const e=list[first*9+k];if(e)map[900+k]=e[1];}
  return{station:null,craft:2,slots,local:{tab:ctab,map},cr:{tabs,sel:ctab,rows,first,scroll:rows>5}};
}
// A busca: uma caixa de texto em cima da do jogo (82, 6), filtra no celular.
const sbox=document.createElement("input");sbox.id="sbox";sbox.maxLength=50;sbox.setAttribute("enterkeyhint","search");
const sview=document.createElement("div");sview.className="sview";
let blinkOn=true,lastSearchDraw="";
// O texto da busca como o jogo desenha: o fim que cabe em 80 de largura e o cursor.
function drawSearch(){
  if(!sview.isConnected)return;
  let t=sbox.value;while(t&&measure(t+"_")>80)t=t.slice(1);
  const cur=document.activeElement===sbox&&blinkOn?"_":"",key=t+"|"+cur+"|"+sview.dataset.k;
  if(key===lastSearchDraw)return;lastSearchDraw=key;
  mc(sview,t+cur||" ",{s:+sview.dataset.k||2,c:"#ffffff"});
}
setInterval(()=>{blinkOn=!blinkOn;drawSearch();},300);
// O navegador rola a página para mostrar o campo quando o teclado sobe: a página
// fica parada (o campo já está no topo) e Enter fecha o teclado.
const unscroll=()=>{if(window.scrollX||window.scrollY)window.scrollTo(0,0);const se=document.scrollingElement;if(se&&se.scrollTop)se.scrollTop=0;};
sbox.addEventListener("focus",()=>{blinkOn=true;unscroll();setTimeout(unscroll,50);setTimeout(unscroll,300);drawSearch();});
sbox.addEventListener("blur",()=>{unscroll();drawSearch();});
sbox.addEventListener("keydown",e=>{if(e.key==="Enter"){e.preventDefault();sbox.blur();}});
if(window.visualViewport){visualViewport.addEventListener("resize",unscroll);visualViewport.addEventListener("scroll",unscroll);}
window.addEventListener("scroll",unscroll);
const SQ={seq:0,set:null,setQ:"",waiting:false};
function searchFor(q){
  const seq=++SQ.seq;SQ.waiting=!!q;
  if(!q){SQ.set=null;SQ.setQ="";return;}
  send(`sq ${seq} ${q}`);
  setTimeout(()=>{if(SQ.seq===seq&&SQ.waiting){SQ.waiting=false;SQ.set=null;if(lastState)onState(lastState);}},600);
}
function onFound(j){
  if(j.q!==SQ.seq)return;
  SQ.waiting=false;SQ.set=new Set(j.r);SQ.setQ=cquery;cfirst[ctab]=0;if(lastState)onState(lastState);
}
sbox.addEventListener("input",()=>{blinkOn=true;drawSearch();cquery=sbox.value;cfirst[ctab]=0;searchFor(cquery);if(lastState)onState(lastState);});
sbox.addEventListener("pointerdown",e=>e.stopPropagation());
function setSearch(on){if(!on){cquery="";sbox.value="";sbox.remove();searchFor("");}}

function skinOf(w){
  const ids=new Set(w.slots.map(s=>s[0]));
  if(w.cr){const tab=w.cr.tabs.find(t=>t[0]===w.cr.sel),inv=ids.has(500);
    const search=tab&&(tab[5]?tab[5]==="search":/compass/.test(tab[1]));
    const bg=inv?"tab_inventory":(search?"tab_item_search":"tab_items");
    return{bg:"container/creative_inventory/"+bg+".png",w:195,h:136,cre:true,inv,search,player:inv?[73,6,32,43]:null,labels:inv||!tab?[]:[[8,6,tab[2]]]};}
  const container=[...ids].some(i=>i>=300&&i<500);
  if(!container&&!w.station&&w.craft===2&&ids.has(500))
    return{bg:"container/inventory.png",w:176,h:166,player:[26,8,49,70],labels:[[97,8,L("container.crafting",tr("crafting"))]]};
  if(!container&&w.craft===3&&(!w.station||w.station==="Crafting"))
    return{bg:"container/crafting_table.png",w:176,h:166,labels:[[29,6,L("container.crafting",tr("crafting"))],[8,72,L("container.inventory",tr("inventory"))]]};
  return null;
}
let winKey="",slotEls=new Map(),winSlots=[],winScale=2,tabsKey="",crState=null,winLocal=null;
// Abas com os sprites do jogo: x = 27 × coluna (a coluna 6 encosta à direita),
// em cima em y = -28, embaixo em y = 132; o ícone em (x+5, y+9 ou y+7).
function renderSkinTabs(cr,k){
  const key=JSON.stringify([cr.tabs,cr.sel,k]);if(key===tabsKey)return;tabsKey=key;
  const win=$("win");win.querySelectorAll(".tabx").forEach(e=>e.remove());
  cr.tabs.forEach(([i,icon,title,top,col])=>{
    const sel=i===cr.sel,x=col===6?195-27+1:27*col,y=top?-28:132;
    const t=document.createElement("div");t.className="tabx";
    t.style.cssText=`left:${x*k}px;top:${y*k}px;width:${26*k}px;height:${32*k}px;z-index:${sel?3:-1};background-image:url(${GUI}sprites/container/creative_inventory/tab_${top?"top":"bottom"}_${sel?"selected":"unselected"}_${col+1}.png)`;
    const ic=document.createElement("div");ic.className="tico";
    ic.style.cssText=`left:${5*k}px;top:${(top?9:7)*k}px;width:${16*k}px;height:${16*k}px`;
    t.appendChild(ic);if(icon)drawIcon(ic,icon,16*k);
    t.addEventListener("pointerdown",e=>{e.stopPropagation();if(CAT.tabs)chooseTab(i);else{send("ct "+i);buzz(8);showTip(null,title);}});
    win.appendChild(t);
  });
}
// A barra de rolagem: trilha em (174, 18), 14 × 112; o puxador (12 × 15) desce
// (112 - 17) × posição. Arrastar o puxador ou tocar na trilha rola.
function renderScroll(cr,k){
  const win=$("win");let s=win.querySelector(".scr");
  if(!s){s=document.createElement("div");s.className="scr";s.innerHTML='<div class="thumb"></div>';win.appendChild(s);
    let id=null;
    const go=e=>{const r=s.getBoundingClientRect(),c=crState;if(!c||!c.scroll)return;
      const hidden=Math.max(1,c.rows-5),f=Math.min(1,Math.max(0,((e.clientY-r.top)/winScale-7.5)/(112-15)));
      const d=Math.max(-9,Math.min(9,c.first-Math.round(f*hidden)));
      if(d){c.first-=d;scrollRows(d);placeThumb(c,winScale);buzz(4);}};
    s.addEventListener("pointerdown",e=>{e.stopPropagation();e.preventDefault();id=e.pointerId;s.setPointerCapture(id);go(e);});
    s.addEventListener("pointermove",e=>{if(e.pointerId===id)go(e);});
    const end=e=>{if(e.pointerId===id)id=null;};s.addEventListener("pointerup",end);s.addEventListener("pointercancel",end);}
  s.style.cssText=`left:${174*k}px;top:${18*k}px;width:${14*k}px;height:${112*k}px`;
  placeThumb(cr,k);
}
function placeThumb(cr,k){const th=$("win").querySelector(".scr .thumb");if(!th)return;
  const hidden=Math.max(1,cr.rows-5),f=cr.scroll?Math.min(1,cr.first/hidden):0;
  th.style.cssText=`left:${1*k}px;top:${Math.floor(95*f)*k}px;width:${12*k}px;height:${15*k}px;background-image:url(${GUI}sprites/container/creative_inventory/${cr.scroll?"scroller":"scroller_disabled"}.png)`;}
function renderWin(w){
  const win=$("win"),wrap=$("winWrap"),skin=skinOf(w);
  document.body.classList.toggle("cre",!!w.cr);
  winLocal=w.local||null;
  const key=(skin?skin.bg:"css")+"|"+w.slots.map(s=>s[0]+":"+s[1]+":"+s[2]).join(",");
  if(key!==winKey){
    winKey=key;tabsKey="";win.dataset.labels="";win.querySelectorAll(".sl,.arrow,.tabx,.lbl,.scr,.bin,.pbox").forEach(e=>e.remove());slotEls.clear();
    win.classList.toggle("skin",!!skin);
    let ox,oy,bw,bh,padTop=0,padBot=0;
    if(skin){ox=0;oy=0;bw=skin.w;bh=skin.h;if(skin.cre){padTop=28;padBot=28;}}
    else{let x0=1e9,y0=1e9,x1=-1e9,y1=-1e9;
      w.slots.forEach(([i,x,y])=>{x0=Math.min(x0,x);y0=Math.min(y0,y);x1=Math.max(x1,x+18);y1=Math.max(y1,y+18);});
      const pad=7,top=13;ox=x0-pad;oy=y0-pad-top;bw=x1-x0+pad*2;bh=y1-y0+pad*2+top;}
    const k=Math.max(1,Math.min((wrap.clientWidth-6)/bw,(wrap.clientHeight-6)/(bh+padTop+padBot)));winScale=k;
    win.style.width=bw*k+"px";win.style.height=bh*k+"px";win.style.margin=`${padTop*k}px 0 ${padBot*k}px`;
    win.style.background=skin?`url(${GUI+skin.bg}) 0 0/${256*k}px ${256*k}px no-repeat`:"";
    const S=18*k;
    w.slots.forEach(([i,x,y])=>{
      const el=document.createElement("div");el.className="sl"+(i===200&&!skin?" big":"");
      el.style.left=((x-ox-1)*k)+"px";el.style.top=((y-oy-1)*k)+"px";el.style.width=S+"px";el.style.height=S+"px";
      win.appendChild(el);slotEls.set(i,{el,st:undefined,size:S});slotTouch(el,i);
    });
    if(skin){
      if(skin.player){const[px,py,pw,ph]=skin.player,b=document.createElement("div");b.className="pbox";
        b.style.cssText=`left:${px*k}px;top:${py*k}px;width:${pw*k}px;height:${ph*k}px`;win.appendChild(b);renderPlayer(b,pw*k,ph*k);}
      if(skin.inv){const b=document.createElement("div");b.className="bin";b.style.cssText=`left:${172*k}px;top:${111*k}px;width:${18*k}px;height:${18*k}px`;
        b.addEventListener("pointerdown",e=>{e.stopPropagation();send("d");buzz(20);});win.appendChild(b);}
    }else{
      const grid=w.slots.filter(s=>s[0]>=100&&s[0]<109),res=w.slots.find(s=>s[0]===200);
      if(grid.length&&res){const gx=Math.max(...grid.map(s=>s[1]))+18,ys=grid.map(s=>s[2]),gy=(Math.min(...ys)+Math.max(...ys))/2;
        const a=document.createElement("div");a.className="arrow";a.innerHTML=`<svg viewBox="0 0 8 8" style="width:100%;height:100%;shape-rendering:crispEdges">${[[0,3,6,2],[4,1,1,6],[5,2,1,4],[6,3,1,2]].map(([x,y,w,h])=>`<rect x="${x}" y="${y}" width="${w}" height="${h}" fill="#8b8b8b"/>`).join("")}</svg>`;
        const ax=(gx+res[1])/2-9;a.style.left=((ax-ox)*k)+"px";a.style.top=((gy-oy)*k)+"px";a.style.width=a.style.height=(18*k)+"px";win.appendChild(a);}
    }
  }
  // Os rótulos (título da aba, "Fabricação"...) mudam sem a grade mudar: redesenha quando o texto muda.
  const lk=skin?JSON.stringify([skin.labels,winScale]):"";
  if(lk!==win.dataset.labels){win.dataset.labels=lk;win.querySelectorAll(".lbl").forEach(e=>e.remove());
    if(skin)skin.labels.forEach(([x,y,text])=>{const d=document.createElement("div");d.className="lbl";d.style.left=x*winScale+"px";d.style.top=y*winScale+"px";win.appendChild(d);mc(d,text,{s:winScale,c:"#404040",noShadow:true});});}
  crState=w.cr?{...w.cr}:null;
  if(skin&&skin.cre){renderSkinTabs(w.cr,winScale);if(!skin.inv)renderScroll(w.cr,winScale);}
  // A caixa de busca, na aba de busca, por cima da do jogo.
  if(skin&&skin.search&&w.local){const k=winScale;
    // O campo de verdade fica invisível (é ele que abre o teclado); o texto é
    // desenhado como o EditBox do jogo: fonte do jogo, branco, cursor "_".
    sbox.style.cssText=`position:absolute;left:${80*k}px;top:${4*k}px;width:${92*k}px;height:${13*k}px;font-size:16px;background:transparent;border:0;outline:none;color:transparent;caret-color:transparent;z-index:3`;
    sview.style.cssText=`position:absolute;left:${82*k}px;top:${6*k}px;pointer-events:none;z-index:2`;sview.dataset.k=k;
    if(!sbox.isConnected){win.appendChild(sbox);win.appendChild(sview);}drawSearch();}
  else if(sbox.isConnected){sbox.remove();sview.remove();}
  if(!skin){const title=w.station?stationName(w.station):w.craft===3?G("container.crafting","crafting"):G("container.inventory","inventory");
    const t=$("winTitle");if(t.dataset.t!==title)mc(t,title,{s:Math.max(1,Math.round(winScale)),c:"#404040",noShadow:true});}
  winSlots=w.slots;
  w.slots.forEach(([i,,,st])=>{const s=slotEls.get(i);const k=JSON.stringify(st);if(s&&s.st!==k){s.st=k;fillSlot(s.el,st,s.size);}});
}
// Um clique num slot: os da grade do catálogo vão como "gi aba posição", o resto
// como clique no slot da janela do jogo.
function clickSlot(i,right,shift){
  if(winLocal&&i>=900){const idx=winLocal.map[i];if(idx!==undefined)send(`gi ${winLocal.tab} ${idx} ${right?1:0} ${shift?1:0}`);return;}
  send(`k ${i} ${right?1:0} ${shift?1:0}`);
}
// ---- O item na mão flutua sob o dedo, como o do cursor no PC -----------------
// Arrastar de um slot pega o item e solta onde o dedo sair (fora da janela,
// volta para a origem). Na grade do criativo, o arrasto começa depois de
// segurar um instante: o deslize rápido continua rolando a lista.
const floatEl=$("float");let floatSt="",dragging=false,dragSt=null;
function floatAt(x,y){floatEl.style.left=x+"px";floatEl.style.top=y+"px";}
function floatShow(st){
  const k=JSON.stringify(st||null);document.body.classList.toggle("carry",!!st);
  if(k===floatSt)return;floatSt=k;
  const S=Math.round(18*winScale*1.2);floatEl.style.width=floatEl.style.height=S+"px";
  floatEl.innerHTML="";if(st){const b=document.createElement("div");b.style.cssText="position:relative;width:100%;height:100%";floatEl.appendChild(b);fillSlot(b,st,S);}
}
// Como no Minecraft de celular: o item só segue o dedo enquanto ele encosta na
// tela; ao soltar, some (fica no painel "Na mão") até o toque no destino.
const touching=new Set();
const touchEnd=e=>{touching.delete(e.pointerId);document.body.classList.toggle("touching",touching.size>0);};
document.addEventListener("pointerdown",e=>{touching.add(e.pointerId);document.body.classList.add("touching");if(mode==="inv")floatAt(e.clientX,e.clientY);},true);
document.addEventListener("pointerup",touchEnd,true);document.addEventListener("pointercancel",touchEnd,true);
document.addEventListener("pointermove",e=>{if(mode==="inv"&&e.buttons)floatAt(e.clientX,e.clientY);},true);
function slotAt(x,y){const e=document.elementFromPoint(x,y),sl=e&&e.closest&&e.closest(".sl");if(!sl)return null;
  for(const[i,s]of slotEls)if(s.el===sl)return i;return null;}
// Um slot no celular, como o inventário do Minecraft de celular:
//  - toque: pega / coloca (o clique do jogo; "Metade / um" e "Mover rápido" valem);
//  - segurar (0,25 s) ou arrastar: o item sobe, segue o dedo e cai onde o dedo
//    sair: num slot (ocupado: troca, e o de lá vai para a origem), na lixeira
//    (apaga) ou fora da janela (volta). O slot debaixo do dedo acende;
//  - na grade do criativo, deslizar rápido rola a lista (arrastar pede segurar).
function dropTargetAt(x,y){
  const under=document.elementFromPoint(x,y);
  if(under&&under.closest){const t=under.closest(".bin,#trash");if(t)return{trash:t};}
  const s=slotAt(x,y);return s===null?null:{slot:s,el:slotEls.get(s).el};
}
function slotTouch(el,i){
  let timer=null,start=null,drag=false,moved=false,lit=null;
  const light=t=>{const e=t&&(t.el||t.trash);if(e===lit)return;if(lit)lit.classList.remove("hit","on");lit=e||null;if(lit)lit.classList.add(t.trash?"on":"hit");};
  const stackHere=()=>(winSlots.find(s=>s[0]===i)||[])[3];
  const lift=(x,y)=>{
    const st=stackHere();if(!st||(lastState&&lastState.car))return false;   // nada aqui, ou a mão já está cheia
    drag=true;dragging=true;dragSt=st;el.classList.remove("hit");
    clickSlot(i,halfMode,false);
    floatShow(halfMode?[st[0],Math.ceil(st[1]/2),...st.slice(2)]:st);floatAt(x,y);buzz(15);return true;
  };
  const finish=()=>{clearTimeout(timer);timer=null;el.classList.remove("hit");light(null);start=null;};
  el.addEventListener("pointerdown",e=>{
    e.preventDefault();drag=false;moved=false;start={x:e.clientX,y:e.clientY};el.classList.add("hit");
    // O toque fica preso ao slot, não ao ícone: o ícone é redesenhado quando o
    // item sai dele, e sem isso o "soltar" se perdia (o item ficava na mão).
    try{el.setPointerCapture(e.pointerId);}catch(err){}
    timer=setTimeout(()=>{timer=null;if(start&&!swiped&&!moved)lift(start.x,start.y);},250);
  });
  el.addEventListener("pointermove",e=>{
    if(!start)return;
    if(drag){floatAt(e.clientX,e.clientY);light(dropTargetAt(e.clientX,e.clientY));return;}
    if(moved)return;
    if(Math.hypot(e.clientX-start.x,e.clientY-start.y)<10)return;
    moved=true;clearTimeout(timer);timer=null;el.classList.remove("hit");
    // Fora da grade do criativo, arrastar já levanta o item; na grade, é rolagem.
    if(!(winLocal&&i>=900))lift(e.clientX,e.clientY);
  });
  el.addEventListener("pointerup",e=>{
    if(drag){
      drag=false;dragging=false;finish();
      const t=dropTargetAt(e.clientX,e.clientY);
      if(t&&t.trash){send("d");buzz(20);showTip(null);return;}
      const to=t?t.slot:i;
      clickSlot(to,false,false);buzz(12);
      // Ocupado por outro item: o jogo troca e o de lá vem para a mão; ele vai para
      // a origem (na grade do criativo, some, como soltar na grade).
      const there=to===i?null:(winSlots.find(s=>s[0]===to)||[])[3];
      if(there&&!(dragSt&&there[0]===dragSt[0]&&JSON.stringify(there.slice(3))===JSON.stringify(dragSt.slice(3))))clickSlot(i,false,false);
      showTip(to===i?null:dragSt,null,to===i?null:`s ${to}`);
      return;
    }
    const tap=start&&!moved&&!swiped;finish();
    if(!tap)return;
    clickSlot(i,halfMode,shiftMode);buzz(8);
    const st=stackHere(),idx=winLocal&&i>=900?winLocal.map[i]:undefined;
    showTip(st,null,idx!==undefined?`c ${winLocal.tab} ${idx}`:`s ${i}`);
  });
  el.addEventListener("pointercancel",()=>{if(drag){drag=false;dragging=false;clickSlot(i,false,false);}finish();});
}
function stackText(st){return st?`${itemName(st[0],st[4])}${st[1]>1?" x"+st[1]:""}`:null;}
// O painel da direita: o item na mão; senão o último tocado; senão a dica.
let tipCarried=null,tipLast=null,tipText=null,tipKey="";
// A dica do jogo (nome e linhas com cor) do item mostrado: pedida ao tocar.
const TT={seq:0,lines:null,of:null};
function askTip(of){TT.of=of;TT.lines=null;if(!of)return;const seq=++TT.seq;TT.want=seq;send(`tt ${seq} ${of}`);}
function onTip(j){if(j.q!==TT.want)return;TT.lines=j.l&&j.l.length?j.l:null;tipKey="";updateTip();}
function showTip(st,text,of){tipLast=st||null;tipText=text||null;if(!tipCarried)askTip(st?of:null);updateTip();}
let lastCarried="";
function carried(st){const k=JSON.stringify(st);if(k===lastCarried)return;lastCarried=k;tipCarried=st||null;
  askTip(st?"m":(tipLast?TT.of:null));updateTip();}
function updateTip(){
  const st=tipCarried||tipLast,key=JSON.stringify([tipCarried,tipLast,tipText,Object.keys(names).length>0,LANG,TT.lines]);if(key===tipKey)return;tipKey=key;
  const box=$("tipbox").querySelector(".box"),nm=$("tipbox").querySelector(".nm"),sub=$("tipbox").querySelector(".sub");
  box.style.display=st?"":"none";fillSlot(box,st,48);
  const tw=Math.max(60,$("tipbox").clientWidth-20);
  if(st){
    // Com a dica do jogo: o nome na cor da raridade e as linhas de baixo
    // (encantamentos de um livro, efeitos de uma poção...) com as cores delas.
    const L=TT.lines;
    mc(nm,L?L[0][0]:itemName(st[0],st[4]),{s:2,w:tw,fit:true,c:L?"#"+L[0][1]:"#ffffff"});
    sub.innerHTML="";delete sub.dataset.t;
    const extra=(L||[]).slice(1).filter(l=>l[0].trim());
    for(const[text,c]of extra.slice(0,8)){const d=document.createElement("div");sub.appendChild(d);mc(d,text,{s:2,c:"#"+c,w:tw,fit:true});}
    const more=tipCarried?(st[1]>1?`${tr("hand")} · ${st[1]}`:tr("hand")):(st[1]>1?tr("items",st[1]):"");
    if(more){const d=document.createElement("div");sub.appendChild(d);mc(d,more,{s:2,c:"#a8a8a8",w:tw,fit:true});}}
  else if(tipText){mc(nm,tipText,{s:2,w:tw,fit:true});mc(sub,"",{s:1});}
  else{mc(nm,tr("tip1"),{s:2,w:tw,c:"#ffffff",fit:true});mc(sub,tr("tip2"),{s:2,c:"#a8a8a8",w:tw,fit:true});}
}
updateTip();

// ---- Modos: jogo, menu e inventário ------------------------------------------
function setMode(m){if(m===mode)return;const was=mode;mode=m;
  document.body.classList.toggle("menu",m==="menu");document.body.classList.toggle("inv",m==="inv");
  if(was)release();if(m!=="inv")winKey="";}
setMode("game");
let lastState=null;
function onState(s){
  lastState=s;loadCatalog(s.cv);if(s.lg){if(s.lg!==LANG)setLang(s.lg);try{localStorage.setItem("cc.glang",s.lg);}catch(e){}}setSkin(s.sk,s.sl);preload(s);
  setMode(s.sc==="playing"?"game":(s.win?"inv":"menu"));
  target=s.tg||null;
  (s.hb||[]).forEach((st,i)=>{const k=JSON.stringify(st);if(hs[i].dataset.k!==k){hs[i].dataset.k=k;fillSlot(hs[i],st,40);}hs[i].classList.toggle("sel",i===s.sel);});
  // No criativo não há vida: os corações somem, como no jogo.
  hearts(s.hp==null||s.cm?null:Math.round(s.hp));
  if(s.win)renderWin(s.win.cr&&CAT.tabs?localCreative(s.win):s.win);else setSearch(false);
  carried(s.car);if(!dragging)floatShow(mode==="inv"?s.car:null);
}

// ---- Chat --------------------------------------------------------------------
function openChat(on){document.body.classList.toggle("chat",on);if(on){release();setTimeout(()=>$("chatIn").focus(),50);const l=$("chatLog");l.scrollTop=l.scrollHeight;}else $("chatIn").blur();}
$("chatBtn").addEventListener("click",()=>{openChat(true);buzz(10);});
$("chatClose").addEventListener("click",()=>openChat(false));
$("chatForm").addEventListener("submit",e=>{e.preventDefault();const t=$("chatIn").value.trim();if(t){send("t "+t);$("chatIn").value="";buzz(10);}});
function line(x){if(typeof x==="string")return x;if(x&&typeof x==="object")return x.text||x.plain||x.line||JSON.stringify(x);return String(x);}
let chatKey="";
function onChat(list){const k=JSON.stringify(list);if(k===chatKey)return;chatKey=k;
  const l=$("chatLog"),atEnd=l.scrollTop+l.clientHeight>=l.scrollHeight-20,w=Math.max(200,l.clientWidth-20);
  l.innerHTML="";[...list].reverse().forEach(x=>{const d=document.createElement("div");l.appendChild(d);mc(d,line(x),{s:2,w});});
  if(atEnd)l.scrollTop=l.scrollHeight;}

// ---- Vibração como a de um controle ------------------------------------------
function rumble(strong,weak,ms){
  const f=Math.min(1,Math.max(strong,weak*0.7));if(f<=0||ms<=0)return;
  if(f>=0.65){buzz(ms);return;}
  const on=Math.max(3,Math.round(18*f)),off=Math.max(4,18-on),pat=[];
  for(let t=0;t<ms;t+=on+off)pat.push(on,off);buzz(pat);
}

// ---- Tela de carregamento ------------------------------------------------------
// Antes de liberar o controle, baixa e monta tudo o que o celular desenha: fonte,
// idioma, o catálogo do criativo, os modelos de todos os itens (um pacote só) e as
// texturas. Assim nenhum item aparece pela metade. Da segunda vez quase tudo vem
// do cache do navegador. Sem catálogo ainda (fora de um mundo), libera depois do
// resto e monta o criativo em segundo plano quando ele chegar.
const PRE={cv:null,running:false,shown:false};
function loadStep(key,frac){if(key)mc($("loadStep"),tr(key),{s:2,c:"#a8a8a8"});$("loadBar").firstChild.style.width=Math.round(Math.max(0,Math.min(1,frac))*100)+"%";}
function itemsOf(s){
  const ids=new Set();
  for(const st of s.hb||[])if(st)ids.add(st[0]);
  if(s.car)ids.add(s.car[0]);
  for(const t of CAT.tabs||[])for(const e of t[6]||[])ids.add(String(e).split("|")[0]);
  return[...ids];
}
// As texturas que um item usa (as mesmas URLs que o desenho vai pedir).
function texturesOf(d,info){
  const out=[];
  if(d&&d.kind==="flat")for(const l of d.layers)out.push(l.t);
  else if(d&&d.kind==="3d")for(const p of d.parts){
    if(p.tex)out.push(p.tex);
    for(const el of p.elements||[])for(const f of Object.values(el.faces||{})){const t=p.res(String(f.texture||"").replace(/^#/,""));if(t)out.push(t);}
  }
  else if(info&&info.k==="flat")out.push(info.t);
  else if(info&&info.k==="cube")out.push(info.t,info.f,info.s);
  return out.filter(Boolean);
}
async function inBatches(list,size,fn,progress){
  let done=0;
  for(let i=0;i<list.length;i+=size){
    await Promise.all(list.slice(i,i+size).map(x=>Promise.resolve(fn(x)).catch(()=>{})));
    done=Math.min(list.length,i+size);progress(done/list.length);
  }
}
async function preload(s){
  const cv=s.cv||"";
  if(PRE.running||PRE.cv===cv)return;
  PRE.running=true;PRE.cv=cv;
  // A primeira vez cobre a tela; as seguintes (o catálogo que chegou depois,
  // ou um idioma novo) montam em segundo plano.
  const cover=!PRE.shown;PRE.shown=true;
  if(cover){document.body.classList.add("loading");release();}
  const safety=cover?setTimeout(()=>document.body.classList.remove("loading"),30000):0;
  try{
    loadStep("ldFont",0.02);await Promise.all([FONTP,LANGP]);
    loadStep("ldCat",0.06);if(cv)await loadCatalog(cv);
    loadStep("ldModels",0.12);await Promise.all([loadBundle(),entityModels()]);
    const ids=itemsOf(s);const descs=new Map();
    loadStep("ldItems",0.2);
    await inBatches(ids,60,async id=>{descs.set(id,await modelDesc(id));},f=>loadStep(null,0.2+0.25*f));
    const tex=new Set();
    for(const id of ids){try{const d=descs.get(id);const info=d?null:await iconInfo(id);for(const t of texturesOf(d,info))tex.add(t);}catch(e){}}
    loadStep("ldTex",0.45);
    await inBatches([...tex],16,t=>texDim(t),f=>loadStep(null,0.45+0.55*f));
  }catch(e){}
  finally{
    clearTimeout(safety);PRE.running=false;
    if(cover){loadStep(null,1);setTimeout(()=>document.body.classList.remove("loading"),150);}
    // Chegou um catálogo novo enquanto carregava: monta ele também.
    if(lastState&&(lastState.cv||"")!==PRE.cv)preload(lastState);
  }
}

// ---- Rede ----------------------------------------------------------------------
let ws=null,lastSend=0,pingT=0;
let key=new URLSearchParams(location.search).get("k")||"";
let stKey="connecting";
function status(ok,key,raw){stKey=raw?null:key;const text=raw?key:tr(key);$("st").classList.toggle("ok",ok);if($("stt").dataset.t!==text)mc($("stt"),text,{s:2});}
function send(m){if(ws&&ws.readyState===1)ws.send(m);}
function connect(){
  status(false,"connecting");
  ws=new WebSocket(`ws://${location.host}/ws?k=${encodeURIComponent(key)}`);
  ws.onopen=()=>{status(true,"connected");dirty=true;};
  ws.onmessage=e=>{const m=String(e.data),c=m[0],body=m.slice(2);
    if(c==="p"){const ping=Math.round(performance.now()-(+body));status(true,ping+" ms",true);send("l "+ping);}
    else if(c==="s"){try{onState(JSON.parse(body));}catch(err){}}
    else if(c==="c"){try{onChat(JSON.parse(body));}catch(err){}}
    else if(c==="f"){try{onFound(JSON.parse(body));}catch(err){}}
    else if(c==="v"){try{onTip(JSON.parse(body));}catch(err){}}
    else if(c==="r"){const[a,b,ms]=body.split(" ").map(Number);rumble(a,b,ms);}
    else if(m==="x"){ws.onclose=null;ws=null;status(false,"taken");show("takenLong");}
  };
  ws.onclose=()=>{status(false,"offline");ws=null;setTimeout(connect,1000);};
  ws.onerror=()=>{};
}
const f=v=>(Math.round(v*1000)/1000).toString();
function tick(t){
  lookStep(t);
  if(ws&&ws.readyState===1){
    if(dirty||t-lastSend>200){ws.send(`i ${f(state.lx)} ${f(state.ly)} ${f(state.rx)} ${f(state.ry)} ${f(state.lt)} ${f(state.rt)} ${state.btn}`);dirty=false;lastSend=t;}
    if(t-pingT>1000){pingT=t;ws.send("p "+Math.round(t));}
  }
  requestAnimationFrame(tick);
}
requestAnimationFrame(tick);

// Tela cheia deitada no primeiro toque.
const over=$("over");
let overKey="";
function show(key){overKey=key;mc($("overt"),tr(key),{s:2,w:Math.min(460,innerWidth-40)});over.style.display="flex";}
show("start");
// Sem o código na URL (o celular não leu o QR): o jogador digita o da sala, e
// o servidor confere (403 com o código errado) antes de ligar.
const codeIn=$("code");
async function askCode(){
  const c=codeIn.value.trim().toLowerCase();
  if(!c){codeIn.focus();return false;}
  let ok=false;try{ok=(await fetch(`/ws?k=${encodeURIComponent(c)}`,{cache:"no-store"})).status!==403;}catch(e){}
  if(!ok){show("codeBad");buzz(60);codeIn.select();return false;}
  key=c;codeIn.style.display="none";
  try{history.replaceState(null,"",location.pathname+"?k="+encodeURIComponent(c));}catch(e){}
  return true;
}
codeIn.addEventListener("keydown",e=>{if(e.key==="Enter"){e.preventDefault();$("go").click();}});
$("go").addEventListener("click",async()=>{
  if(!key&&!(await askCode()))return;
  over.style.display="none";
  try{await document.documentElement.requestFullscreen({navigationUI:"hide"});}catch(e){}
  try{await screen.orientation.lock("landscape");}catch(e){}
  if(!ws)connect();
});
if(!key){show("codeAsk");codeIn.style.display="";}
document.addEventListener("visibilitychange",()=>{if(document.hidden)release();});
document.addEventListener("contextmenu",e=>e.preventDefault());
window.addEventListener("resize",()=>{winKey="";});
// ?demo: sem a capa nem conexão, com estado de exemplo (para tirar print da página).
const q=new URLSearchParams(location.search);
if(q.has("demo")){over.style.display="none";
  const hb=[["minecraft:diamond_pickaxe",1,0],["minecraft:oak_log",32,0],["minecraft:grass_block",64,0],["minecraft:torch",16,0],null,["minecraft:bread",5,0],["minecraft:oak_stairs",8,0],null,["minecraft:oak_leaves",12,0]];
  const slots=[];for(let i=0;i<36;i++){const x=8+(i%9)*18,y=i<9?142:84+Math.floor((i-9)/9)*18;slots.push([i,x,y,i<9?hb[i]:(i===12?["minecraft:cobblestone",40,0]:null)]);}
  const craft=q.has("bancada")?3:2;
  for(let g=0;g<craft*craft;g++){const r=Math.floor(g/craft),c=g%craft;slots.push([100+g,craft===3?30+c*18:98+c*18,craft===3?17+r*18:18+r*18,g===0?["minecraft:oak_planks",4,0]:null]);}
  slots.push([200,craft===3?124:154,craft===3?35:28,["minecraft:crafting_table",1,0]]);
  if(craft===2)for(let a=0;a<4;a++)slots.push([500+a,8,8+a*18,null]);
  const run=()=>{onState({sc:q.has("inv")?"inventory":q.has("menu")?"paused":"playing",sel:1,hb,hp:15,car:q.has("inv")?["minecraft:stick",3,0]:null,win:q.has("inv")?{craft,station:craft===3?"Crafting":null,slots}:null});
    onChat(["<Player> oi do celular, ação e coração","[Server] Bem-vindo ao mundo"]);status(true,"12 ms",true);};
  if(q.has("cr")){const items=q.has("itens")?q.get("itens").split(",").map(x=>x.includes(":")?x:"minecraft:"+x):["minecraft:oak_log","minecraft:stone","minecraft:bricks","minecraft:oak_planks","minecraft:grass_block","minecraft:glass","minecraft:cobblestone","minecraft:sand","minecraft:oak_leaves","minecraft:torch","minecraft:diamond_pickaxe","minecraft:bread"];
    const cs=[];for(let i=0;i<45;i++)cs.push([900+i,9+(i%9)*18,18+Math.floor(i/9)*18,[items[i%items.length],1,0]]);
    for(let i=0;i<9;i++)cs.push([i,9+i*18,112,hb[i]]);
    const tabs=[[0,"minecraft:bricks","Blocos de construção",true,0],[1,"minecraft:cyan_wool","Blocos coloridos",true,1],[2,"minecraft:grass_block","Blocos naturais",true,2],[3,"minecraft:crafting_table","Blocos funcionais",true,3],[4,"minecraft:redstone","Redstone",true,4],[5,"minecraft:bookshelf","Hotbars salvas",true,5],[6,"minecraft:compass","Pesquisar",true,6],
      [7,"minecraft:diamond_pickaxe","Ferramentas",false,0],[8,"minecraft:netherite_sword","Combate",false,1],[9,"minecraft:golden_carrot","Comida",false,2],[10,"minecraft:iron_ingot","Ingredientes",false,3],[11,"minecraft:pig_spawn_egg","Ovos geradores",false,4],[12,"minecraft:chest","Inventário de sobrevivência",false,6]];
    // Catálogo de exemplo (como viria de /creative.json), para a navegação local.
    CAT.tabs=tabs.map(t=>[...t,t[0]===12?"inv":t[0]===6?"search":"items",Array.from({length:t[0]===6?300:120},(_,n)=>items[(n+t[0])%items.length])]);CAT.v="demo";
    if(q.has("busca"))ctab=6;
    // ?aba=N: troca de aba depois de desenhar (como um toque na aba).
    if(q.has("aba"))setTimeout(()=>chooseTab(+q.get("aba")),2000);
    // ?hit: confere o que recebe o toque no meio de cada aba (vai para o título da página).
    if(q.has("hit"))setTimeout(()=>{document.title=[...document.querySelectorAll(".tabx")].map(t=>{const r=t.getBoundingClientRect();
      const e=document.elementFromPoint(r.left+r.width/2,r.top+r.height/2);return t.contains(e)?"ok":"X:"+(e&&(e.id||e.className));}).join(",");},2500);
    const run2=()=>{onState({sc:"creative",cm:true,sel:1,hb,hp:20,car:null,win:{craft:2,station:null,slots:cs,cr:{tabs,sel:0,rows:40,first:0,scroll:true}}});status(true,"12 ms",true);};
    run2();setTimeout(()=>{winKey="";tabsKey="";run2();},1500);
  }else{run();setTimeout(()=>{lastCarried="";winKey="";run();},1500);}
  if(q.has("chat"))openChat(true);
  if(q.has("fwd"))dpad.classList.add("fwd");
  if(q.has("edit")){if(q.has("moved"))layout={dpad:{x:40,y:-30,s:1.3,o:.7},jump:{x:-60,y:0,s:.8,o:1}};applyLayout();$("editBtn").click();}
  if(q.has("stick")){lstick.style.left="610px";lstick.style.top="220px";lstick.style.display="block";lknob.style.transform="translate(34px,-22px)";$("hint").style.display="none";}
}
</script>
</body>
</html>
"####;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_and_texts() {
        assert_eq!(short_address("http://192.168.1.172:8790/?k=384e065c"), "192.168.1.172:8790");
        assert_eq!(short_address(""), "");
        assert_eq!(text("pt_br", "code"), "Código");
        assert_eq!(text("ja_jp", "connect"), "Connect phone (show QR)");
        assert_eq!(text("es_es", "off"), "No");
    }

    #[test]
    fn local_da_skin_como_o_jogo() {
        // O hash da URL da skin vira o mesmo nome que o jogo usa no disco.
        assert_eq!(
            skin_location("http://textures.minecraft.net/texture/abc"),
            format!("skins/{}", sha1(&[b'a', 0, b'b', 0, b'c', 0]).iter().map(|b| format!("{b:02x}")).collect::<String>())
        );
        assert!(skin_file("skins/../../x").is_none());
    }

    #[test]
    fn pacote_de_modelos_junta_os_jsons() {
        let dir = std::env::temp_dir().join(format!("pocketpad-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("items")).unwrap();
        fs::create_dir_all(dir.join("models").join("block")).unwrap();
        fs::write(dir.join("items").join("stone.json"), "{\"model\": {\"type\": \"model\"}}\n").unwrap();
        fs::write(dir.join("models").join("block").join("stone.json"), "\u{feff}{\"parent\": \"block/cube_all\"}").unwrap();
        fs::write(dir.join("models").join("block").join("bad.json"), "nada").unwrap();
        let b = build_bundle(&dir);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            b,
            "{\"items/stone.json\":{\"model\": {\"type\": \"model\"}},\"models/block/stone.json\":{\"parent\": \"block/cube_all\"}}"
        );
    }

    #[test]
    fn sha1_e_base64_conferem() {
        let hex: String = sha1(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn aceite_do_websocket_e_o_da_rfc() {
        assert_eq!(ws_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn input_e_limitado() {
        let i = parse_input("i 0.5 -2 0 1 0 1.5 5").unwrap();
        assert_eq!(i.lx, 0.5);
        assert_eq!(i.ly, -1.0);
        assert_eq!(i.rt, 1.0);
        assert_eq!(i.buttons, 5);
        assert!(parse_input("i 1 2").is_none());
        assert!(parse_input("i NaN 0 0 0 0 0 0").is_none());
        assert!(parse_input("x 0 0 0 0 0 0 0").is_none());
    }

    #[test]
    fn diff_so_manda_o_que_mudou() {
        let a = Input::default();
        let b = Input {
            lx: 0.5,
            ly: 1.0,
            rt: 1.0,
            buttons: 1 | (1 << 11),
            ..Input::default()
        };
        assert_eq!(
            diff(&a, &b),
            [
                "pad stick left 0.500 1.000",
                "pad trigger right 1.000",
                "pad south down",
                "pad rs down",
            ]
        );
        assert_eq!(diff(&b, &b), Vec::<String>::new());
        assert_eq!(diff(&b, &a).last().unwrap(), "pad rs up");
    }

    #[test]
    fn quadro_do_websocket_ida_e_volta() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x1, b"i 0 0 0 0 0 0 0").unwrap();
        // O servidor manda sem máscara; o leitor aceita os dois.
        let (op, payload) = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(op, 1);
        assert_eq!(payload, b"i 0 0 0 0 0 0 0");
        // Quadro mascarado, como o navegador manda.
        let mask = [1u8, 2, 3, 4];
        let text = b"p 12";
        let mut framed = vec![0x81, 0x80 | text.len() as u8];
        framed.extend_from_slice(&mask);
        framed.extend(text.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        let (_, payload) = read_frame(&mut framed.as_slice()).unwrap();
        assert_eq!(payload, text);
    }

    #[test]
    fn pedido_http() {
        let (path, query, headers) =
            parse_request("GET /ws?k=ab12 HTTP/1.1\r\nUpgrade: websocket\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(path, "/ws");
        assert_eq!(query, "k=ab12");
        assert_eq!(headers[0], ("Upgrade".to_owned(), "websocket".to_owned()));
        assert!(parse_request("POST / HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn qr_tem_o_tamanho_e_os_localizadores() {
        let m = qr("http://192.168.100.200:8790/?k=1a2b3c4d").unwrap();
        // 39 bytes: versão 3 (29x29).
        assert_eq!(m.len(), 29);
        // Centro dos três localizadores escuro, anel branco em volta.
        for (x, y) in [(3, 3), (25, 3), (3, 25)] {
            assert!(m[y][x]);
            assert!(!m[y][x + 2]);
            assert!(m[y][x + 3]);
        }
        // Módulo escuro fixo.
        assert!(m[29 - 8][8]);
        assert_eq!(qr("a").unwrap().len(), 21);
        assert!(qr(&"x".repeat(200)).is_none());
    }

    #[test]
    fn rs_de_referencia() {
        // "HELLO WORLD" 1-M do tutorial do thonky: correção de 10 codewords.
        let data = [32u8, 91, 11, 120, 209, 114, 220, 77, 67, 64, 236, 17, 236, 17, 236, 17];
        assert_eq!(
            rs_remainder(&data, &rs_divisor(10)),
            [196, 35, 39, 119, 235, 215, 231, 226, 93, 23]
        );
    }

    #[test]
    fn acoes_do_celular() {
        assert_eq!(
            parse_action("k 200 1 0"),
            Some(Action::Click { slot: 200, right: true, shift: false })
        );
        assert_eq!(
            parse_action("k 12 0 1"),
            Some(Action::Click { slot: 12, right: false, shift: true })
        );
        assert_eq!(parse_action("h 3"), Some(Action::Select(3)));
        assert_eq!(parse_action("h 9"), None);
        assert_eq!(parse_action("e"), Some(Action::OpenInventory));
        assert_eq!(parse_action("q"), Some(Action::Close));
        assert_eq!(parse_action("b 1"), Some(Action::Tab(true)));
        assert_eq!(parse_action("t  /time set day "), Some(Action::Chat("/time set day".into())));
        assert_eq!(parse_action("t \u{7}"), None);
        assert_eq!(parse_action("ct 4"), Some(Action::CreativeTab(4)));
        assert_eq!(parse_action("ct 99"), None);
        assert_eq!(parse_action("w -1"), Some(Action::Scroll(-1)));
        assert_eq!(parse_action("w 0"), None);
        assert_eq!(parse_action("d"), Some(Action::Trash));
        assert_eq!(
            parse_action("gi 2 130 0 1"),
            Some(Action::Pick { tab: 2, index: 130, right: false, shift: true })
        );
        assert_eq!(parse_action("gi 2"), None);
        assert_eq!(parse_action("sq 7  anvil"), Some(Action::Search { seq: 7, text: " anvil".into() }));
        assert_eq!(parse_action("sq 8"), Some(Action::Search { seq: 8, text: String::new() }));
        assert_eq!(parse_action("sq x a"), None);
        assert_eq!(parse_action("tt 3 c 2 140"), Some(Action::Tooltip { seq: 3, of: TipOf::Catalog(2, 140) }));
        assert_eq!(parse_action("tt 4 s 36"), Some(Action::Tooltip { seq: 4, of: TipOf::Slot(36) }));
        assert_eq!(parse_action("tt 5 m"), Some(Action::Tooltip { seq: 5, of: TipOf::Carried }));
        assert_eq!(parse_action("tt 6 c 99 1"), None);
        assert_eq!(parse_action("tt 7 x"), None);
        assert_eq!(parse_action("k x"), None);
        assert_eq!(parse_action("z"), None);
    }

    #[test]
    fn caminhos_seguros() {
        assert!(safe_path("block/oak_log.png"));
        assert!(!safe_path("../secret"));
        assert!(!safe_path("/etc/passwd"));
        assert!(!safe_path("block\\x"));
        assert!(!safe_path("Block/X"));
        assert!(!safe_path(""));
    }

    #[test]
    fn leitura_dos_modelos() {
        let block = r##"{ "parent": "minecraft:block/cube_column",
            "textures": { "end": "minecraft:block/oak_log_top", "side": "minecraft:block/oak_log" } }"##;
        assert_eq!(json_str(block, "parent").as_deref(), Some("minecraft:block/cube_column"));
        assert_eq!(
            texture_map(block),
            [
                ("end".to_owned(), "minecraft:block/oak_log_top".to_owned()),
                ("side".to_owned(), "minecraft:block/oak_log".to_owned()),
            ]
        );
        // O primeiro "model" que é string, não o objeto de fora.
        let item = r#"{"model":{"type":"minecraft:model","model":"minecraft:block/oak_log"}}"#;
        assert_eq!(json_str(item, "model").as_deref(), Some("minecraft:block/oak_log"));
        assert!(texture_map("{}").is_empty());
    }

    #[test]
    fn icones_dos_assets() {
        let dir = std::env::temp_dir().join(format!("celular-icones-{}", std::process::id()));
        let w = |p: &str, t: &str| {
            let p = dir.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w("items/oak_log.json", r#"{"model":{"type":"minecraft:model","model":"minecraft:block/oak_log"}}"#);
        w("models/block/oak_log.json", r#"{"parent":"minecraft:block/cube_column","textures":{"end":"minecraft:block/oak_log_top","side":"minecraft:block/oak_log"}}"#);
        w("models/block/cube_column.json", r##"{"parent":"block/cube","textures":{"particle":"#side","up":"#end","north":"#side"}}"##);
        w("items/stick.json", r#"{"model":{"type":"minecraft:model","model":"minecraft:item/stick"}}"#);
        w("models/item/stick.json", r#"{"parent":"minecraft:item/generated","textures":{"layer0":"minecraft:item/stick"}}"#);
        w("models/item/chest.json", r#"{"parent":"minecraft:item/template_chest","textures":{"particle":"minecraft:block/oak_planks"}}"#);
        assert_eq!(
            icon_json(&dir, "minecraft:oak_log"),
            r#"{"k":"cube","t":"block/oak_log_top","s":"block/oak_log","f":"block/oak_log"}"#
        );
        assert_eq!(icon_json(&dir, "minecraft:stick"), r#"{"k":"flat","t":"item/stick"}"#);
        assert_eq!(icon_json(&dir, "chest"), r#"{"k":"flat","t":"block/oak_planks"}"#);
        assert_eq!(icon_json(&dir, "minecraft:nada"), r#"{"k":"none"}"#);
        assert_eq!(icon_json(&dir, "../x"), r#"{"k":"none"}"#);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pump_sem_celular_nao_manda_estado() {
        let t = pump(false);
        assert!(t.actions.is_empty() && !t.snapshot && !t.chat);
    }
}
