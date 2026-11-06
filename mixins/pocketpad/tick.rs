// Uma vez por iteração do loop (no início de `poll_control`, que tem o
// `event_loop` e enxerga os comandos de controle do jogo): aplica o controle,
// executa o que o celular pediu e manda ao celular o estado (hotbar, vida,
// janela aberta) e o chat.
fn pocketpad_tick(
    game: &mut crate::game::Game<'_>,
    event_loop: &winit::event_loop::ActiveEventLoop,
) {
    use crate::game::pocketpad as m;
    let on = game.native_on(m::ID) && game.native_window();
    let tick = m::pump(on);
    for line in &tick.pad {
        let words: Vec<&str> = line.split(' ').collect();
        let _ = game.pad_command(&words);
    }
    let in_window = |game: &crate::game::Game<'_>| {
        matches!(game.screen_name(), "inventory" | "creative")
    };
    for action in tick.actions {
        match action {
            m::Action::Click { slot, right, shift } => {
                if in_window(game) && game.slots().contains(&(slot as usize)) {
                    let slot = slot.to_string();
                    let mut words = vec!["click-slot", slot.as_str()];
                    if right {
                        words.push("right");
                    }
                    if shift {
                        words.push("shift");
                    }
                    let _ = game.station_command(&words);
                }
            }
            m::Action::Select(n) => {
                if game.player.is_some() {
                    game.selected = usize::from(n.min(8));
                }
            }
            m::Action::OpenInventory => {
                if game.screen_name() == "playing" {
                    let _ = game.pad_command(&["pad", "north", "tap"]);
                }
            }
            m::Action::Close => {
                if in_window(game) {
                    let _ = game.station_command(&["close"]);
                }
            }
            m::Action::Chat(text) => {
                // Como o teclado do PC (`ChatScreen.handleChatInput`): histórico,
                // as restrições de chat e os comandos de mods (/pocketpad) antes
                // de ir ao servidor.
                if game.player.is_some() {
                    game.handle_chat_input(&text);
                }
            }
            m::Action::Tab(next) => {
                let _ = game.pad_command(&["pad", if next { "rb" } else { "lb" }, "tap"]);
            }
            m::Action::CreativeTab(i) => {
                let i = usize::from(i);
                if game.screen_name() == "creative" && i < game.creative_screen.tabs.len() {
                    crate::game::select_creative_tab(&mut game.creative_screen, &mut game.inv_clicks, i);
                }
            }
            m::Action::Scroll(rows) => {
                if game.screen_name() == "creative" {
                    let names = game.pack.names.clone();
                    game.creative_screen.wheel(f64::from(rows), &names);
                }
            }
            m::Action::Pick { tab, index, right, shift } => {
                // O mesmo que um clique no slot de exibição do criativo
                // (`creative_display_click`), com a pilha vinda do catálogo.
                if game.screen_name() == "creative" {
                    use horizon_client::creative::{DisplayClick, click_display};
                    use horizon_client::menu::Input;
                    let screen = &game.creative_screen;
                    let stack = screen.tabs.get(usize::from(tab)).and_then(|t| {
                        let list: &[horizon_client::items::ItemStack] =
                            if t.search { screen.search_items() } else { &t.items };
                        list.get(index as usize).cloned()
                    });
                    if let Some(stack) = stack {
                        let input = if shift { Input::QuickMove } else { Input::Pickup };
                        let catalog = world_items!(game);
                        let max = |s: &horizon_client::items::ItemStack| {
                            catalog.get(&s.item).map_or(64, |i| i.max_stack)
                        };
                        match click_display(&mut game.carried, Some(&stack), max, i32::from(right), input) {
                            DisplayClick::Hotbar(i, stack) if i < game.inventory.len() => {
                                game.inventory[i] = Some(stack);
                            }
                            DisplayClick::Offhand(stack) => game.offhand = Some(stack),
                            _ => {}
                        }
                    }
                }
            }
            m::Action::Search { seq, text } => {
                // A busca do jogo (a mesma do PC: nome no idioma, linhas da dica,
                // `#tag`, `minecraft:id`), em posições da aba de busca do catálogo.
                let names = game.pack.names.clone();
                let found = game.creative_screen.search_results(&text, &names);
                m::publish('f', &serde_json::json!({ "q": seq, "r": found }).to_string());
            }
            m::Action::Tooltip { seq, of } => {
                // A dica como o jogo desenha: cada linha com o texto e a cor (RGB).
                let stack = match of {
                    m::TipOf::Catalog(tab, index) => {
                        let screen = &game.creative_screen;
                        screen.tabs.get(usize::from(tab)).and_then(|t| {
                            let list: &[horizon_client::items::ItemStack] =
                                if t.search { screen.search_items() } else { &t.items };
                            list.get(index as usize).cloned()
                        })
                    }
                    m::TipOf::Slot(slot) => {
                        if in_window(game) && game.slots().contains(&(slot as usize)) {
                            game.slot_view(slot as usize)
                        } else {
                            None
                        }
                    }
                    m::TipOf::Carried => game.carried.clone(),
                };
                let lines: Vec<serde_json::Value> = stack
                    .map(|s| {
                        game.stack_tooltip(&s)
                            .into_iter()
                            .map(|(text, c)| {
                                // `rgba` do HUD: 0xAABBGGRR.
                                let rgb = (c & 0xFF) << 16 | (c >> 8 & 0xFF) << 8 | (c >> 16 & 0xFF);
                                serde_json::json!([text.plain(), format!("{rgb:06x}")])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                m::publish('v', &serde_json::json!({ "q": seq, "l": lines }).to_string());
            }
            m::Action::Trash => {
                // A lixeira do criativo: no criativo o item na mão é só do cliente
                // (como no vanilla), então apagar é esvaziar a mão, em qualquer aba.
                if game.screen_name() == "creative" {
                    game.carried = None;
                }
            }
        }
    }
    // O catálogo do criativo, uma vez por sessão: abas e ids dos itens de cada
    // uma. O celular guarda em cache e navega sem perguntar ao jogo.
    if on && game.player.is_some() && m::catalog_needed(&game.settings.language) {
        let screen = &game.creative_screen;
        let mut ids: Vec<String> = Vec::new();
        let tabs: Vec<serde_json::Value> = screen
            .tab_headers()
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let full = &screen.tabs[i];
                let list: &[horizon_client::items::ItemStack] =
                    if full.search { screen.search_items() } else { &full.items };
                // Poções (e flechas com efeito): `id|cor|efeito`, a cor do jogo.
                let items: Vec<String> = list
                    .iter()
                    .map(|s| match s.potion() {
                        Some(p) => format!(
                            "{}|{:06x}|{}",
                            &*s.item,
                            horizon_client::brewing::potion_color(Some(p)),
                            p
                        ),
                        None => (*s.item).to_owned(),
                    })
                    .collect();
                ids.extend(list.iter().map(|s| (*s.item).to_owned()));
                let kind = if h.inventory {
                    "inv"
                } else if h.search {
                    "search"
                } else if h.hotbar {
                    "hotbar"
                } else {
                    "items"
                };
                serde_json::json!([i, &h.icon, game.creative_tab_title(h), h.top, h.column, kind, items])
            })
            .collect();
        ids.extend(screen.tab_headers().iter().map(|h| h.icon.clone()));
        m::set_catalog(serde_json::json!({ "tabs": tabs }).to_string(), ids, &game.settings.language);
    }
    if tick.snapshot {
        // Um slot: [id, quantidade, dano] e, numa poção, a cor e o efeito.
        let stack = |s: &Option<horizon_client::items::ItemStack>| {
            s.as_ref().map(|s| match s.potion() {
                Some(p) => serde_json::json!([
                    &*s.item,
                    s.count,
                    s.damage,
                    format!("{:06x}", horizon_client::brewing::potion_color(Some(p))),
                    p
                ]),
                None => serde_json::json!([&*s.item, s.count, s.damage]),
            })
        };
        let screen = game.screen_name();
        let window = if game.player.is_some() && matches!(screen, "inventory" | "creative") {
            let ids: Vec<usize> = game.slots();
            let menu = game.station_command(&["menu"]).ok().flatten();
            let station = menu
                .as_ref()
                .and_then(|v| v.get("station"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            // Criativo: os slots de exibição (900..) mostram a lista da aba, a
            // partir da primeira linha visível, como o jogo desenha.
            let names = game.pack.names.clone();
            let creative = screen == "creative";
            let first = if creative { game.creative_screen.first_row(&names) } else { 0 };
            let display = horizon_client::creative::Creative::DISPLAY;
            let mut slots = Vec::with_capacity(ids.len());
            for i in ids {
                let (x, y) = game.slot_position(i);
                let view = if creative && i >= display {
                    game.creative_screen.shown(&names, first * 9 + i - display)
                } else {
                    game.slot_view(i)
                };
                slots.push(serde_json::json!([i, x, y, stack(&view)]));
            }
            let tabs = if creative {
                let headers = game.creative_screen.tab_headers();
                let list: Vec<serde_json::Value> = headers
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        serde_json::json!([i, &t.icon, game.creative_tab_title(t), t.top, t.column])
                    })
                    .collect();
                serde_json::json!({
                    "tabs": list,
                    "sel": game.creative_screen.selected,
                    "rows": game.creative_screen.rows(&names),
                    "first": first,
                    "scroll": game.creative_screen.can_scroll(&names),
                })
            } else {
                serde_json::Value::Null
            };
            serde_json::json!({
                "station": station,
                "craft": game.craft_size,
                "slots": slots,
                "cr": tabs,
            })
        } else {
            serde_json::Value::Null
        };
        #[allow(clippy::unnecessary_cast)]
        let health = game.player.as_ref().map(|p| p.health.health() as f32);
        let hotbar: Vec<serde_json::Value> =
            (0..9).map(|i| stack(&game.inventory[i]).unwrap_or_default()).collect();
        // O que está na mira: um toque no celular ataca um mob ou usa um bloco.
        let target = if game.player.is_some() && screen == "playing" {
            let (reply, _rx) = std::sync::mpsc::channel::<String>();
            game.control_command("looking-at", &reply, event_loop)
                .ok()
                .flatten()
                .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        } else {
            None
        };
        // A skin: a que o jogo desenha, se for a baixada da conta; se ele ainda
        // usa uma padrão (sem login, ou antes de baixar), a da conta escolhida
        // no launcher, quando já está no cache em disco.
        static ACCOUNT: std::sync::OnceLock<Option<(String, bool)>> = std::sync::OnceLock::new();
        let drawn = game.player_model.as_ref().map(|p| (p.skin.clone(), p.slim));
        let skin: (Option<String>, bool) = match drawn {
            Some((Some(s), slim)) if s.starts_with("skins/") => (Some(s), slim),
            other => {
                let account = ACCOUNT.get_or_init(|| {
                    let text = m::launcher_accounts()?;
                    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                    let sel = v.get("selected")?.as_str()?;
                    let acc = v.get("accounts")?.as_array()?.iter().find(|a| {
                        a.get("uuid").and_then(|u| u.as_str()) == Some(sel)
                    })?;
                    let skin = acc.get("skin")?.as_array()?;
                    let loc = m::skin_location(skin.first()?.as_str()?);
                    let slim = skin.get(1).and_then(|s| s.as_str()) == Some("slim");
                    m::skin_file(&loc).map(|_| (loc, slim))
                });
                match (account, other) {
                    (Some((loc, slim)), _) => (Some(loc.clone()), *slim),
                    (None, Some((s, slim))) => (s, slim),
                    (None, None) => (None, false),
                }
            }
        };
        let state = serde_json::json!({
            "sc": screen,
            "sel": game.selected,
            "hb": hotbar,
            "hp": health,
            "cm": game.creative,
            "cv": m::catalog_version(),
            "car": stack(&game.carried),
            "tg": target,
            // A skin do jogador como o jogo a desenha: `skins/<sha1>` (a da
            // conta, no cache em disco) ou uma padrão (`entity/player/...`).
            // O idioma do jogo: o celular fala o mesmo.
            "lg": game.settings.language,
            "sk": skin.0,
            "sl": skin.1,
            "win": window,
        });
        m::publish('s', &state.to_string());
    }
    if tick.chat && game.player.is_some() {
        let (reply, _rx) = std::sync::mpsc::channel::<String>();
        if let Ok(Some(value)) = game.control_command("messages 40", &reply, event_loop) {
            if let Some(chat) = value.get("chat") {
                m::publish('c', &chat.to_string());
            }
        }
    }
}
