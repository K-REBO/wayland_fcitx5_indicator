// IME mode indicator for fcitx5
// fcitx5の入力モードを画面中央に表示

use anyhow::{Context, Result};
use std::os::fd::AsFd;
use std::time::Duration;
use std::sync::{Arc, Mutex};

// Waylandクライアントライブラリ
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_compositor, wl_shm, wl_shm_pool, wl_surface, wl_buffer, wl_registry, wl_region},
    globals::{registry_queue_init, GlobalListContents},
};

// Layer Shellプロトコル
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1, Anchor, KeyboardInteractivity},
};

use dbus::blocking::Connection as DbusConnection;
use dbus::message::MatchRule;

fn main() -> Result<()> {
    println!("=== fcitx5 IME Mode Indicator (Daemon) ===\n");
    println!("fcitx5の入力メソッド変更を監視しています...");
    println!("終了するには Ctrl+C を押してください\n");

    // DBus接続を確立
    let dbus_conn = DbusConnection::new_session()
        .context("DBusセッションバスへの接続に失敗")?;

    // 現在の入力メソッドを保存（重複表示を防ぐため）
    let last_input_method = Arc::new(Mutex::new(String::new()));

    // 初回の入力メソッドを取得して表示
    if let Ok(current) = get_current_input_method() {
        println!("初期入力メソッド: {}", current);
        *last_input_method.lock().unwrap() = current.clone();

        let display_text = get_display_text(&current);
        if let Err(e) = std::thread::spawn(move || display_text_overlay(&display_text)).join() {
            eprintln!("表示エラー: {:?}", e);
        }
    }

    // fcitx5のプロパティ変更シグナルをマッチ
    let rule = MatchRule::new_signal("org.fcitx.Fcitx.InputMethod1", "CurrentIMChanged");

    dbus_conn.add_match(rule, move |_: (), _, _| {
        // シグナル受信時の処理
        true
    }).context("マッチルールの追加に失敗")?;

    // 代替案: PropertiesChangedシグナルも監視
    let rule2 = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .with_sender("org.fcitx.Fcitx5");

    let last_im_clone = Arc::clone(&last_input_method);
    dbus_conn.add_match(rule2, move |_: (), _, _| {
        // 入力メソッドが変更されたかチェック
        if let Ok(current) = get_current_input_method() {
            let mut last = last_im_clone.lock().unwrap();
            if *last != current {
                println!("入力メソッド変更: {} -> {}", *last, current);
                *last = current.clone();

                let display_text = get_display_text(&current);
                // 別スレッドで表示（ブロッキングを避ける）
                std::thread::spawn(move || {
                    if let Err(e) = display_text_overlay(&display_text) {
                        eprintln!("表示エラー: {}", e);
                    }
                });
            }
        }
        true
    }).context("マッチルールの追加に失敗")?;

    // fcitx-remoteコマンドの実行を監視する代替手段
    // （より確実に変更を検知）
    println!("✓ DBusシグナル監視を開始しました");

    // メインループ（ポーリング + DBusイベント処理）
    let last_im_poll = Arc::clone(&last_input_method);
    loop {
        // DBusイベント処理（タイムアウト付き）
        dbus_conn.process(Duration::from_millis(500))?;

        // 定期的にポーリングもする（シグナルが来ない場合のフォールバック）
        if let Ok(current) = get_current_input_method() {
            let mut last = last_im_poll.lock().unwrap();
            if *last != current {
                println!("入力メソッド変更: {} -> {}", *last, current);
                *last = current.clone();

                let display_text = get_display_text(&current);
                std::thread::spawn(move || {
                    if let Err(e) = display_text_overlay(&display_text) {
                        eprintln!("表示エラー: {}", e);
                    }
                });
            }
        }
    }
}

/// 入力メソッド名から表示テキストを決定
fn get_display_text(input_method: &str) -> String {
    if input_method == "mozc" {
        "かな".to_string()
    } else {
        "en".to_string()
    }
}

/// fcitx5の現在の入力メソッドをDBusで取得
fn get_current_input_method() -> Result<String> {
    let conn = dbus::blocking::Connection::new_session()
        .context("DBusセッションバスへの接続に失敗")?;

    let proxy = conn.with_proxy(
        "org.fcitx.Fcitx5",
        "/controller",
        Duration::from_millis(5000),
    );

    let (input_method,): (String,) = proxy.method_call(
        "org.fcitx.Fcitx.Controller1",
        "CurrentInputMethod",
        (),
    ).context("fcitx5から入力メソッドの取得に失敗")?;

    Ok(input_method)
}

/// 画面中央にテキストを表示
fn display_text_overlay(text: &str) -> Result<()> {
    // Waylandコンポジタへの接続
    let conn = Connection::connect_to_env()
        .context("Waylandコンポジタへの接続に失敗")?;

    // イベントキューとグローバルの初期化
    let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)
        .context("グローバルレジストリの取得に失敗")?;

    let qh = event_queue.handle();

    // 必要なグローバルをバインド
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositorのバインドに失敗")?;

    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .context("wl_shmのバインドに失敗")?;

    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("zwlr_layer_shell_v1のバインドに失敗")?;

    // サーフェスの作成
    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "modal_ime_indicator".to_string(),
        &qh,
        (),
    );

    // サイズ設定（テキストに応じて調整）
    let width = 300;
    let height = 150;

    layer_surface.set_size(width, height);
    layer_surface.set_anchor(Anchor::empty()); // 画面中央
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(-1);

    // 入力リージョンを空に設定（マウス/タッチ入力を通過させる）
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));

    surface.commit();

    // イベントループで設定を待機
    event_queue.blocking_dispatch(&mut AppState::new())?;

    // 初期表示（即座に表示）
    let buffer = create_text_buffer(&shm, &qh, width as i32, height as i32, text, 1.0)
        .context("テキスト描画バッファの作成に失敗")?;

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width as i32, height as i32);
    surface.commit();

    let mut state = AppState::new();
    event_queue.roundtrip(&mut state)?;

    // 1秒待機
    std::thread::sleep(Duration::from_millis(1000));

    // フェードアウトアニメーション（1秒間、10フレーム）
    let total_frames = 10;
    let frame_duration = Duration::from_millis(100);

    for frame in 1..=total_frames {
        // アルファ値を計算（1.0 -> 0.0）
        let alpha = 1.0 - (frame as f64 / total_frames as f64);

        // バッファを作成
        let buffer = create_text_buffer(&shm, &qh, width as i32, height as i32, text, alpha)
            .context("テキスト描画バッファの作成に失敗")?;

        // バッファをサーフェスにアタッチ
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();

        // イベント処理
        event_queue.roundtrip(&mut state)?;

        // 次のフレームまで待機
        std::thread::sleep(frame_duration);
    }

    Ok(())
}

/// Cairoでテキストを描画した共有メモリバッファを作成
fn create_text_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<AppState>,
    width: i32,
    height: i32,
    text: &str,
    alpha: f64,
) -> Result<wl_buffer::WlBuffer> {
    let stride = width * 4; // ARGB8888 = 4 bytes per pixel
    let size = stride * height;

    // Cairo ImageSurfaceを作成
    let mut cairo_surface = cairo::ImageSurface::create(
        cairo::Format::ARgb32,
        width,
        height,
    )
    .context("Cairo ImageSurfaceの作成に失敗")?;

    // Cairo描画
    {
        let cairo_context = cairo::Context::new(&cairo_surface)
            .context("Cairo Contextの作成に失敗")?;

        // 背景を半透明の暗い色で塗りつぶし（alphaを適用）
        cairo_context.set_source_rgba(0.1, 0.1, 0.1, 0.95 * alpha);
        cairo_context.paint().context("背景描画に失敗")?;

        // 角丸の四角形を描画
        let radius = 20.0;
        let x = 10.0;
        let y = 10.0;
        let w = f64::from(width) - 20.0;
        let h = f64::from(height) - 20.0;

        cairo_context.new_path();
        cairo_context.arc(x + w - radius, y + radius, radius, -std::f64::consts::PI / 2.0, 0.0);
        cairo_context.arc(x + w - radius, y + h - radius, radius, 0.0, std::f64::consts::PI / 2.0);
        cairo_context.arc(x + radius, y + h - radius, radius, std::f64::consts::PI / 2.0, std::f64::consts::PI);
        cairo_context.arc(x + radius, y + radius, radius, std::f64::consts::PI, 3.0 * std::f64::consts::PI / 2.0);
        cairo_context.close_path();

        cairo_context.set_source_rgba(0.2, 0.2, 0.2, 0.95 * alpha);
        cairo_context.fill().context("角丸四角形の描画に失敗")?;

        // テキストを描画
        cairo_context.select_font_face(
            "Sans",
            cairo::FontSlant::Normal,
            cairo::FontWeight::Bold,
        );
        cairo_context.set_font_size(64.0);

        // テキストのサイズを測定して中央配置
        let extents = cairo_context.text_extents(text)
            .context("テキストサイズ測定に失敗")?;

        let text_x = (f64::from(width) - extents.width()) / 2.0 - extents.x_bearing();
        let text_y = (f64::from(height) - extents.height()) / 2.0 - extents.y_bearing();

        // テキストを白色で描画（alphaを適用）
        cairo_context.set_source_rgba(1.0, 1.0, 1.0, alpha);
        cairo_context.move_to(text_x, text_y);
        cairo_context.show_text(text).context("テキスト描画に失敗")?;
    }

    // Cairoサーフェスのデータを取得
    cairo_surface.flush();
    let cairo_data = cairo_surface.data()
        .context("Cairoデータの取得に失敗")?;

    // 一時ファイルを作成（共有メモリ用）
    let file = tempfile::tempfile()
        .context("一時ファイルの作成に失敗")?;

    // ファイルサイズを設定
    nix::unistd::ftruncate(&file, size as i64)
        .context("ファイルサイズの設定に失敗")?;

    // メモリマップ
    let mut mmap = unsafe {
        memmap2::MmapMut::map_mut(&file)
            .context("メモリマップに失敗")?
    };

    // CairoのデータをWaylandバッファにコピー
    mmap.copy_from_slice(&cairo_data);

    // 共有メモリプールを作成
    let pool = shm.create_pool(
        file.as_fd(),
        size,
        qh,
        (),
    );

    // バッファを作成
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );

    pool.destroy();

    Ok(buffer)
}

// アプリケーション状態（イベントハンドラ用）
struct AppState {
    configured: bool,
}

impl AppState {
    fn new() -> Self {
        Self { configured: false }
    }
}

// Waylandイベントディスパッチャの実装
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<wl_region::WlRegion, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                _proxy.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {}
            _ => {}
        }
    }
}
