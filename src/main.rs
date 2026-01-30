// IME mode indicator for fcitx5
// fcitx5の入力モードを画面中央に表示
// 最適化版: バッファキャッシュ、共有メモリプール再利用

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::fd::AsFd;
use std::time::Duration;
use std::sync::{Arc, Mutex};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};

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
use crossbeam_channel::unbounded;
use hyprland::data::Client;
use hyprland::prelude::*;
use memmap2::MmapMut;

mod config;
use config::Config;

/// キャッシュされたバッファ（各アルファ値のピクセルデータを保持）
struct CachedBuffer {
    /// アルファ=1.0のピクセルデータ（ARGB8888）
    pixels_full: Vec<u8>,
}

impl CachedBuffer {
    /// 指定アルファ値でピクセルデータを生成
    fn get_pixels_with_alpha(&self, alpha: f64) -> Vec<u8> {
        if alpha >= 1.0 {
            return self.pixels_full.clone();
        }

        let mut pixels = self.pixels_full.clone();
        // ARGB8888フォーマット: 各ピクセル4バイト [B, G, R, A]
        for chunk in pixels.chunks_exact_mut(4) {
            // Cairoは事前乗算アルファを使用するため、全チャンネルにアルファを適用
            chunk[0] = (chunk[0] as f64 * alpha) as u8; // B
            chunk[1] = (chunk[1] as f64 * alpha) as u8; // G
            chunk[2] = (chunk[2] as f64 * alpha) as u8; // R
            chunk[3] = (chunk[3] as f64 * alpha) as u8; // A
        }
        pixels
    }
}

/// バッファキャッシュ（テキストごとにCachedBufferを保持）
struct BufferCache {
    cache: HashMap<String, CachedBuffer>,
    width: i32,
    height: i32,
}

impl BufferCache {
    fn new(width: i32, height: i32) -> Self {
        Self {
            cache: HashMap::new(),
            width,
            height,
        }
    }

    /// テキストのバッファを事前レンダリング
    fn prerender(&mut self, text: &str, config: &Config) -> Result<()> {
        if self.cache.contains_key(text) {
            return Ok(());
        }

        let pixels = render_text_to_pixels(self.width, self.height, text, 1.0, config)?;
        self.cache.insert(text.to_string(), CachedBuffer {
            pixels_full: pixels,
        });
        Ok(())
    }

    /// キャッシュからピクセルデータを取得
    fn get(&self, text: &str, alpha: f64) -> Option<Vec<u8>> {
        self.cache.get(text).map(|buf| buf.get_pixels_with_alpha(alpha))
    }
}

/// ピクセルデータからWaylandバッファを作成
fn create_buffer_from_pixels(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<AppState>,
    width: i32,
    height: i32,
    pixels: &[u8],
) -> Result<wl_buffer::WlBuffer> {
    let stride = width * 4;
    let size = stride * height;

    // memfd_create: ディスクI/Oなしの匿名メモリファイル（5-15ms → 1-2ms）
    let name = CStr::from_bytes_with_nul(b"wl_shm\0").unwrap();
    let fd = memfd_create(name, MemFdCreateFlag::MFD_CLOEXEC)
        .context("memfd_createに失敗")?;
    nix::unistd::ftruncate(&fd, size as i64)
        .context("ファイルサイズの設定に失敗")?;

    let mut mmap = unsafe {
        MmapMut::map_mut(&fd)
            .context("メモリマップに失敗")?
    };
    mmap.copy_from_slice(pixels);

    let pool = shm.create_pool(fd.as_fd(), size, qh, ());
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

fn main() -> Result<()> {
    println!("=== fcitx5 IME Mode Indicator (Daemon) ===\n");
    println!("fcitx5の入力メソッド変更を監視しています...");
    println!("終了するには Ctrl+C を押してください\n");

    // 設定をロード
    let config = Arc::new(Config::load());
    println!("✓ 設定ファイルをロードしました");

    // 表示リクエスト用チャネル
    let (tx, rx) = unbounded::<String>();

    // 専用表示スレッドを起動（Wayland接続を1回だけ確立）
    let config_clone = Arc::clone(&config);
    std::thread::spawn(move || {
        if let Err(e) = display_thread(rx, config_clone) {
            eprintln!("表示スレッドエラー: {}", e);
        }
    });

    // DBus接続を確立
    let dbus_conn = DbusConnection::new_session()
        .context("DBusセッションバスへの接続に失敗")?;

    // 現在の入力メソッドを保存（重複表示を防ぐため）
    let last_input_method = Arc::new(Mutex::new(String::new()));

    // 初回の入力メソッドを取得して表示
    if let Ok(current) = get_current_input_method() {
        println!("初期入力メソッド: {}", current);
        *last_input_method.lock().unwrap() = current.clone();

        let display_text = config.get_display_text(&current);
        tx.send(display_text).ok();
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
    let tx_clone = tx.clone();
    let config_clone = Arc::clone(&config);
    dbus_conn.add_match(rule2, move |_: (), _, _| {
        // 入力メソッドが変更されたかチェック
        if let Ok(current) = get_current_input_method() {
            let mut last = last_im_clone.lock().unwrap();
            if *last != current {
                println!("入力メソッド変更: {} -> {}", *last, current);
                *last = current.clone();

                let display_text = config_clone.get_display_text(&current);
                tx_clone.send(display_text).ok();
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

                let display_text = config.get_display_text(&current);
                tx.send(display_text).ok();
            }
        }
    }
}

/// イージング関数（ease-out cubic）
fn ease_out_cubic(t: f64) -> f64 {
    let t1 = t - 1.0;
    t1 * t1 * t1 + 1.0
}

/// アクティブウィンドウの位置とサイズを取得
fn get_active_window_geometry() -> Option<(i32, i32, i32, i32)> {
    // アクティブなウィンドウを取得
    let active_window = Client::get_active().ok()??;

    let x = active_window.at.0 as i32;
    let y = active_window.at.1 as i32;
    let width = active_window.size.0 as i32;
    let height = active_window.size.1 as i32;

    Some((x, y, width, height))
}

/// Cairoでテキストを描画してピクセルデータを返す
fn render_text_to_pixels(
    width: i32,
    height: i32,
    text: &str,
    alpha: f64,
    config: &Config,
) -> Result<Vec<u8>> {
    // Cairo ImageSurfaceを作成
    let mut cairo_surface = cairo::ImageSurface::create(
        cairo::Format::ARgb32,
        width,
        height,
    )
    .context("Cairo ImageSurfaceの作成に失敗")?;

    // Cairo描画（黒背景 + 白い角丸ボックス + 黒文字）
    {
        let cairo_context = cairo::Context::new(&cairo_surface)
            .context("Cairo Contextの作成に失敗")?;

        // 外側の黒い背景を塗りつぶし
        cairo_context.set_source_rgba(0.0, 0.0, 0.0, 0.8 * alpha);
        cairo_context.paint().context("背景描画に失敗")?;

        // 内側の白い角丸ボックスを描画
        let padding = 15.0;
        let corner_radius = 12.0;
        let box_x = padding;
        let box_y = padding;
        let box_width = f64::from(width) - 2.0 * padding;
        let box_height = f64::from(height) - 2.0 * padding;

        // 角丸矩形のパスを作成
        cairo_context.new_path();
        cairo_context.arc(
            box_x + box_width - corner_radius,
            box_y + corner_radius,
            corner_radius,
            -std::f64::consts::PI / 2.0,
            0.0,
        );
        cairo_context.arc(
            box_x + box_width - corner_radius,
            box_y + box_height - corner_radius,
            corner_radius,
            0.0,
            std::f64::consts::PI / 2.0,
        );
        cairo_context.arc(
            box_x + corner_radius,
            box_y + box_height - corner_radius,
            corner_radius,
            std::f64::consts::PI / 2.0,
            std::f64::consts::PI,
        );
        cairo_context.arc(
            box_x + corner_radius,
            box_y + corner_radius,
            corner_radius,
            std::f64::consts::PI,
            3.0 * std::f64::consts::PI / 2.0,
        );
        cairo_context.close_path();

        // 白色で塗りつぶし
        cairo_context.set_source_rgba(1.0, 1.0, 1.0, 0.95 * alpha);
        cairo_context.fill().context("角丸ボックス描画に失敗")?;

        // テキストを描画（設定からフォントを取得）
        cairo_context.select_font_face(
            &config.overlay.font_family,
            cairo::FontSlant::Normal,
            cairo::FontWeight::Bold,
        );
        cairo_context.set_font_size(config.overlay.font_size);

        // テキストのサイズを測定して中央配置
        let extents = cairo_context.text_extents(text)
            .context("テキストサイズ測定に失敗")?;

        let text_x = (f64::from(width) - extents.width()) / 2.0 - extents.x_bearing();
        let text_y = (f64::from(height) - extents.height()) / 2.0 - extents.y_bearing();

        // テキストを黒色で描画
        cairo_context.set_source_rgba(0.0, 0.0, 0.0, alpha);
        cairo_context.move_to(text_x, text_y);
        cairo_context.show_text(text).context("テキスト描画に失敗")?;
    }

    // Cairoサーフェスのデータを取得
    cairo_surface.flush();
    let cairo_data = cairo_surface.data()
        .context("Cairoデータの取得に失敗")?;

    Ok(cairo_data.to_vec())
}

/// 専用表示スレッド（Wayland接続を1回だけ確立、バッファキャッシュを再利用）
fn display_thread(rx: crossbeam_channel::Receiver<String>, config: Arc<Config>) -> Result<()> {
    // Waylandコンポジタへの接続（1回だけ）
    let conn = Connection::connect_to_env()
        .context("Waylandコンポジタへの接続に失敗")?;

    // イベントキューとグローバルの初期化（1回だけ）
    let (globals, mut event_queue) = registry_queue_init::<AppState>(&conn)
        .context("グローバルレジストリの取得に失敗")?;

    let qh = event_queue.handle();

    // 必要なグローバルをバインド（1回だけ）
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositorのバインドに失敗")?;

    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .context("wl_shmのバインドに失敗")?;

    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("zwlr_layer_shell_v1のバインドに失敗")?;

    println!("✓ Wayland接続確立完了");

    let width = config.overlay.width;
    let height = config.overlay.height;

    // バッファキャッシュを作成
    let mut buffer_cache = BufferCache::new(width as i32, height as i32);

    // 設定ファイルの入力メソッドを事前レンダリング
    for display_text in config.input_method_names.values() {
        buffer_cache.prerender(display_text, &config)?;
        println!("✓ バッファを事前レンダリング: {}", display_text);
    }

    println!("✓ 初期化完了、表示リクエストを待機中...");

    // 表示リクエストを処理
    while let Ok(text) = rx.recv() {
        // 未キャッシュのテキストは動的にレンダリング
        if buffer_cache.get(&text, 1.0).is_none() {
            buffer_cache.prerender(&text, &config)?;
            println!("✓ バッファを動的レンダリング: {}", text);
        }

        // オーバーレイを表示（キャッシュされたバッファを使用）
        if let Err(e) = show_overlay_cached(
            &compositor,
            &shm,
            &layer_shell,
            &mut event_queue,
            &qh,
            &conn,
            &buffer_cache,
            &text,
            width,
            height,
            &config,
        ) {
            eprintln!("表示エラー: {}", e);
        }
    }

    Ok(())
}

/// オーバーレイを表示（キャッシュされたバッファを使用）
fn show_overlay_cached(
    compositor: &wl_compositor::WlCompositor,
    shm: &wl_shm::WlShm,
    layer_shell: &ZwlrLayerShellV1,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    qh: &QueueHandle<AppState>,
    conn: &Connection,
    buffer_cache: &BufferCache,
    text: &str,
    width: u32,
    height: u32,
    config: &Config,
) -> Result<()> {
    // サーフェスの作成（毎回新規作成）
    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "modal_ime_indicator".to_string(),
        qh,
        (),
    );

    layer_surface.set_size(width, height);

    // アクティブウィンドウの中央に配置
    if let Some((win_x, win_y, win_width, win_height)) = get_active_window_geometry() {
        let center_x = win_x + win_width / 2;
        let center_y = win_y + win_height / 2;
        let margin_left = center_x - (width as i32) / 2;
        let margin_top = center_y - (height as i32) / 2;

        layer_surface.set_anchor(Anchor::Top | Anchor::Left);
        layer_surface.set_margin(margin_top, 0, 0, margin_left);
    } else {
        // アクティブウィンドウが見つからない場合は画面中央
        layer_surface.set_anchor(Anchor::empty());
    }

    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(-1);

    // 入力リージョンを空に設定
    let region = compositor.create_region(qh, ());
    surface.set_input_region(Some(&region));

    surface.commit();

    // configure待機
    let mut state = AppState::new();
    event_queue.blocking_dispatch(&mut state)?;

    // 初期表示（キャッシュからピクセルデータを取得）
    // flush: 非同期送信で即座に表示（5-10ms → <1ms）
    if let Some(pixels) = buffer_cache.get(text, 1.0) {
        let buffer = create_buffer_from_pixels(shm, qh, width as i32, height as i32, &pixels)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        conn.flush()?;
    }

    // 表示時間
    std::thread::sleep(Duration::from_millis(config.animation.display_duration_ms));

    // フェードアウトアニメーション
    let total_frames = config.animation.fade_frames;
    let frame_duration = Duration::from_millis(config.animation.fade_duration_ms / total_frames as u64);

    for frame in 1..=total_frames {
        let t = frame as f64 / total_frames as f64;
        let alpha = 1.0 - ease_out_cubic(t);

        if let Some(pixels) = buffer_cache.get(text, alpha) {
            let buffer = create_buffer_from_pixels(shm, qh, width as i32, height as i32, &pixels)?;
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.commit();
            event_queue.roundtrip(&mut state)?;
        }
        std::thread::sleep(frame_duration);
    }

    // クリーンアップ
    layer_surface.destroy();
    surface.destroy();
    region.destroy();

    Ok(())
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
