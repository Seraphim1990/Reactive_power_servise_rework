use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::meta::state::main_data::MainDataSignal;
use configparser::ini::Ini;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{Instant, interval_at, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};
use url::Url;

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const BIT_PERIOD: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

// ---------- публічні типи ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Online,
    Offline,
    Unknown,
}

/// Заміна parseNodeState() — підлаштуйте під свої стани.
impl NodeState {
    pub fn parse(s: &str) -> Self {
        match s {
            "online" | "Online" => NodeState::Online,
            "offline" | "Offline" => NodeState::Offline,
            _ => NodeState::Unknown,
        }
    }
}

pub type IncomeData = HashMap<String, Option<f64>>;
pub type IncomeDataPtr = Arc<IncomeData>;
pub type NodeChanges = HashMap<i32, NodeState>;
pub type NodeChangesPtr = Arc<NodeChanges>;

#[derive(Debug, Clone, Default)]
pub struct Subscription {
    pub tags: Vec<String>,
    pub nodes: Vec<i32>,
}

// ---------- конфіг ----------

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Config {
    /// Читає setting/settings.ini (секція [General], як у QSettings без групи).
    /// Логін/пароль можна перевизначити змінними оточення API_USER / API_PASS.
    pub fn load(path: &str) -> Result<Self, String> {
        let mut ini = Ini::new();
        ini.load(path)?;
        let host = ini.get("General", "hostaddr").ok_or("hostAddr відсутній")?;
        let port = ini
            .getuint("General", "hostport")?
            .ok_or("hostPort відсутній")?;
        let port = u16::try_from(port).map_err(|_| "hostPort поза діапазоном u16")?;
        Ok(Self {
            host,
            port,
            // Облікові дані задаються через середовище.
            username: std::env::var("API_USER").map_err(|_| "API_USER відсутній")?,
            password: std::env::var("API_PASS").map_err(|_| "API_PASS відсутній")?,
        })
    }
}

// ---------- хендл (аналог публічного інтерфейсу APIClient) ----------

enum Command {
    Start,
    Subscribe(Subscription),
    /// Аналог AlarmController::throwTagState -> onLiveTextMessageReceived
    InjectMessage(String),
}

#[derive(Clone)]
pub struct ApiClient {
    cmd_tx: mpsc::UnboundedSender<Command>,
    data_tx: broadcast::Sender<IncomeDataPtr>,
    node_tx: broadcast::Sender<NodeChangesPtr>,
    get_tags_tx: broadcast::Sender<()>,
    server_err_rx: watch::Receiver<bool>,
}

impl ApiClient {
    /// Створює клієнт і спавнить фонового актора. Викликати всередині tokio runtime.
    pub fn spawn(
        cfg: Config,
        signal_tx: mpsc::UnboundedSender<MainDataSignal>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (data_tx, _) = broadcast::channel(64);
        let (node_tx, _) = broadcast::channel(64);
        let (get_tags_tx, _) = broadcast::channel(4);
        let (err_tx, server_err_rx) = watch::channel(true); // m_serverErr = true

        let actor = Actor {
            signal_tx,
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap(),
            cfg,
            cmd_rx,
            data_tx: data_tx.clone(),
            node_tx: node_tx.clone(),
            err_tx,
            sub: Subscription::default(),
            access_token: String::new(),
            refresh_token: String::new(),
            closed: false,
        };
        let task = tokio::spawn(actor.run());

        (
            Self {
                cmd_tx,
                data_tx,
                node_tx,
                get_tags_tx,
                server_err_rx,
            },
            task,
        )
    }

    /// emit getTags() — просить споживача прислати підписку
    pub fn run_api_worker(&self) {
        let _ = self.cmd_tx.send(Command::Start);
    }
    pub fn on_get_tags(&self) -> broadcast::Receiver<()> {
        self.get_tags_tx.subscribe()
    }

    /// receiveSubscibe()
    pub fn subscribe(&self, sub: Subscription) {
        let _ = self.cmd_tx.send(Command::Subscribe(sub));
    }
    /// onLiveTextMessageReceived() ззовні (для AlarmController)
    pub fn inject_message(&self, msg: impl Into<String>) {
        let _ = self.cmd_tx.send(Command::InjectMessage(msg.into()));
    }

    pub fn data(&self) -> broadcast::Receiver<IncomeDataPtr> {
        self.data_tx.subscribe()
    }
    pub fn nodes(&self) -> broadcast::Receiver<NodeChangesPtr> {
        self.node_tx.subscribe()
    }
    /// serverErr + serverErrChanged
    pub fn server_err(&self) -> watch::Receiver<bool> {
        self.server_err_rx.clone()
    }
}

// ---------- актор ----------

enum AuthResult {
    Ok,
    Relogin,
    Retry,
}
enum Mode {
    Login,
    Refresh,
}

struct Actor {
    signal_tx: mpsc::UnboundedSender<MainDataSignal>,
    cfg: Config,
    http: reqwest::Client,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    data_tx: broadcast::Sender<IncomeDataPtr>,
    node_tx: broadcast::Sender<NodeChangesPtr>,
    err_tx: watch::Sender<bool>,
    sub: Subscription,
    access_token: String,
    refresh_token: String,
    closed: bool,
}

impl Actor {
    fn base(&self) -> String {
        format!("http://{}:{}", self.cfg.host, self.cfg.port)
    }

    fn set_server_err(&self, v: bool) {
        self.err_tx.send_if_modified(|cur| {
            if *cur != v {
                *cur = v;
                true
            } else {
                false
            }
        });
    }

    async fn run(mut self) {
        // Чекаємо першу підписку (в оригіналі логін стартує з receiveSubscibe)
        while self.sub.tags.is_empty() && self.sub.nodes.is_empty() {
            match self.cmd_rx.recv().await {
                Some(Command::Start) => break,
                Some(c) => {
                    self.apply(c);
                }
                None => return,
            }
        }

        let mut mode = Mode::Login;
        while !self.closed {
            let res = match mode {
                Mode::Login => self.login().await,
                Mode::Refresh => self.refresh().await,
            };
            match res {
                AuthResult::Ok => {
                    self.set_server_err(false);
                    self.session().await;
                    // після обриву: 2 с на "відпочинок" мережі, потім refresh
                    mode = Mode::Refresh;
                    self.wait(RECONNECT_DELAY).await;
                }
                AuthResult::Relogin => mode = Mode::Login,
                AuthResult::Retry => self.wait(RETRY_DELAY).await,
            }
        }
    }

    /// Спить, але обробляє команди (щоб підписка не губилась).
    async fn wait(&mut self, d: Duration) {
        let timer = sleep(d);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                _ = &mut timer => return,
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(c) => { self.apply(c); }
                    None => { self.closed = true; return; }
                },
            }
        }
    }

    /// Застосовує команду; повертає повідомлення, яке треба відправити у ws.
    fn apply(&mut self, cmd: Command) -> Option<String> {
        match cmd {
            Command::Start => None,
            Command::Subscribe(mut s) => {
                // tags.removeDuplicates() зі збереженням порядку
                let mut seen = std::collections::HashSet::new();
                s.tags.retain(|t| seen.insert(t.clone()));
                s.nodes.sort_unstable();
                s.nodes.dedup();
                self.sub = s;
                Some(self.subscription_message())
            }
            Command::InjectMessage(m) => {
                self.process_message(&m);
                None
            }
        }
    }

    fn subscription_message(&self) -> String {
        unimplemented!("Визначити теги та формат підписки API")
    }

    // ----- HTTP авторизація -----

    async fn login(&mut self) -> AuthResult {
        let url = format!("{}/auth/login", self.base());
        let body = json!({ "username": self.cfg.username, "password": self.cfg.password });
        let resp = match self.http.post(url).json(&body).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!("login: HTTP {}", r.status());
                return AuthResult::Retry;
            }
            Err(e) => {
                warn!("login: {e}");
                return AuthResult::Retry;
            }
        };
        match resp.json::<Value>().await {
            Ok(v) if v.is_object() => {
                self.access_token = v["access_token"].as_str().unwrap_or_default().into();
                self.refresh_token = v["refresh_token"].as_str().unwrap_or_default().into();
                AuthResult::Ok
            }
            _ => AuthResult::Retry,
        }
    }

    async fn refresh(&mut self) -> AuthResult {
        let url = format!("{}/auth/refresh_token", self.base());
        let body = json!({ "refresh_token": self.refresh_token });
        let resp = match self.http.post(url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("refresh: {e}");
                return AuthResult::Retry;
            }
        };
        let status = resp.status().as_u16();
        if status == 200 {
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            let token = v["access_token"].as_str().unwrap_or_default();
            if token.is_empty() {
                return AuthResult::Relogin;
            }
            self.access_token = token.into();
            return AuthResult::Ok;
        }
        if status == 401 || status == 403 {
            return AuthResult::Relogin;
        }
        // TODO: лічильник спроб, щоб не крутитись вічно
        AuthResult::Retry
    }

    // ----- WebSocket-сесія -----

    /// Повертається, коли з'єднання втрачено (або heartbeat впав).
    async fn session(&mut self) {
        let mut url = match Url::parse_with_params(
            &format!("ws://{}:{}/live_data", self.cfg.host, self.cfg.port),
            &[("token", self.access_token.as_str())],
        ) {
            Ok(u) => u,
            Err(e) => {
                warn!("bad url: {e}");
                return;
            }
        };
        url.set_fragment(None);

        let (mut ws, _) = match connect_async(url.as_str()).await {
            Ok(x) => x,
            Err(e) => {
                warn!("ws connect: {e}");
                return;
            }
        };
        info!("websocket connected");

        // liveWebSocketConnected(): відправляємо підписку
        if ws
            .send(Message::text(self.subscription_message()))
            .await
            .is_err()
        {
            return;
        }

        let mut bit = interval_at(Instant::now() + BIT_PERIOD, BIT_PERIOD);

        loop {
            tokio::select! {
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(t))) => self.process_message(&t),
                    Some(Ok(Message::Close(_))) | None => { info!("ws closed"); return; }
                    Some(Ok(_)) => {} // ping/pong/binary
                    Some(Err(e)) => { warn!("ws error: {e}"); return; }
                },
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(c) => {
                        if let Some(m) = self.apply(c) {
                            if ws.send(Message::text(m)).await.is_err() { return; }
                        }
                    }
                    None => { self.closed = true; let _ = ws.close(None).await; return; }
                },
                _ = bit.tick() => {
                    if !self.send_bit().await {
                        // onBitRecieved: помилка -> serverErr, стоп таймера, abort сокета
                        self.set_server_err(true);
                        let _ = ws.close(None).await;
                        return;
                    }
                }
            }
        }
    }

    async fn send_bit(&self) -> bool {
        match self.http.get(format!("{}/bit", self.base())).send().await {
            Ok(r) => r.status().as_u16() == 200,
            Err(e) => {
                debug!("bit: {e}");
                false
            }
        }
    }

    // ----- розбір повідомлень -----

    fn process_message(&self, text: &str) {
        for signal in self.on_text_message_received(text) {
            if self.signal_tx.send(signal).is_err() {
                warn!("Канал телеметрії закритий");
                break;
            }
        }
    }

    /// Тут зіставити теги API з MainDataSignal. Відсутні/невалідні значення
    /// не перетворювати на нуль і не надсилати як достовірну телеметрію.
    fn on_text_message_received(&self, _text: &str) -> Vec<MainDataSignal> {
        unimplemented!("Зіставити теги API з MainDataSignal")
    }
}
