use std::sync::{Arc, RwLock};

use askama::Template;
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{Redirect, Response},
    routing::{get, post},
};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::{
    audit,
    model::Model,
    settings,
    store::Store,
    web::{WebError, database, render},
};

mod key;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Environment,
    Cloud,
    Local,
    Rules,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Collaboration {
    #[default]
    Coordinated,
    StrategyOnly,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Config {
    mode: Mode,
    collaboration: Collaboration,
    endpoint: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<SavedApiKey>,
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedApiKey {
    endpoint: String,
    encrypted: String,
}

impl Config {
    fn endpoint(&self, defaults: Option<&Model>) -> Result<Option<Url>, &'static str> {
        match self.mode {
            Mode::Rules => Ok(None),
            Mode::Environment => Ok(defaults.map(|model| model.endpoint().clone())),
            Mode::Cloud | Mode::Local => {
                let url = Url::parse(&self.endpoint).map_err(|_| "请输入有效的模型接口 URL。")?;
                let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
                if (matches!(self.mode, Mode::Cloud) && url.scheme() != "https")
                    || (matches!(self.mode, Mode::Local) && !loopback)
                {
                    return Err("云端模型使用 HTTPS。本地模型使用回环地址。");
                }
                Ok(Some(url))
            }
        }
    }
}

#[derive(Clone)]
struct Active {
    config: Config,
    model: Option<Arc<Model>>,
    key_unreadable: bool,
}

#[derive(Clone)]
pub(crate) struct Agents {
    defaults: Option<Arc<Model>>,
    active: Arc<RwLock<Active>>,
    changes: Arc<tokio::sync::Mutex<()>>,
    store: Store,
}

impl Agents {
    pub(crate) fn initialize(
        store: Store,
        model: Option<Model>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let defaults = model.map(Arc::new);
        let db = store.connect()?;
        let config: Config = settings::read(&db, "agents")?;
        let (model, key_unreadable) = match Self::resolve(&defaults, &config) {
            Ok(model) => (model, false),
            Err(key::UNREADABLE) => (None, true),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            defaults,
            active: Arc::new(RwLock::new(Active {
                config,
                model,
                key_unreadable,
            })),
            changes: Arc::new(tokio::sync::Mutex::new(())),
            store,
        })
    }

    fn resolve(
        defaults: &Option<Arc<Model>>,
        config: &Config,
    ) -> Result<Option<Arc<Model>>, &'static str> {
        let Some(url) = config.endpoint(defaults.as_deref())? else {
            return Ok(None);
        };
        let saved_key = config
            .api_key
            .as_ref()
            .filter(|saved| saved.endpoint == url.as_str());
        if matches!(config.mode, Mode::Environment) && saved_key.is_none() {
            return Ok(defaults.clone());
        }
        let external = matches!(config.mode, Mode::Cloud | Mode::Environment);
        let key = if let Some(saved) = saved_key {
            Some(key::unprotect(&saved.encrypted, url.as_str())?)
        } else if external
            && std::env::var("CITYLINE_AGENT_API_KEY_ENDPOINT").as_deref() == Ok(url.as_str())
        {
            std::env::var("CITYLINE_AGENT_API_KEY").ok()
        } else if external
            && defaults
                .as_ref()
                .is_some_and(|model| model.endpoint() == &url)
        {
            std::env::var("ACCEPTANCE_RUST_MEDIA_MODEL_KEY").ok()
        } else {
            None
        };
        if matches!(config.mode, Mode::Cloud) && key.as_ref().is_none_or(String::is_empty) {
            return Err("请填写此云端接口的 API Key，或为该地址配置服务端密钥。");
        }
        let name = if matches!(config.mode, Mode::Environment) {
            defaults.as_ref().expect("resolved server endpoint").name()
        } else {
            &config.name
        };
        Model::connect(url.as_str(), name.to_owned(), key, external)
            .map(|model| Some(Arc::new(model)))
    }

    pub(crate) fn current(&self) -> (Option<Arc<Model>>, Collaboration) {
        let active = self.active.read().expect("agent configuration lock");
        (active.model.clone(), active.config.collaboration)
    }
}

#[derive(Template)]
#[template(path = "agents.html")]
struct AgentPage {
    heading: &'static str,
    section: &'static str,
    tab: &'static str,
    mode: &'static str,
    collaboration: &'static str,
    endpoint: String,
    name: String,
    configured: bool,
    environment_configured: bool,
    can_save_key: bool,
    key_saved: bool,
    key_unreadable: bool,
    probe: Option<bool>,
    saved: bool,
}

#[derive(Deserialize)]
struct StatusQuery {
    probe: Option<u8>,
    saved: Option<u8>,
    tab: Option<String>,
}

async fn page(
    State(agents): State<Agents>,
    axum::extract::Query(query): axum::extract::Query<StatusQuery>,
) -> Response {
    let active = agents.active.read().expect("agent configuration lock");
    let endpoint = active
        .config
        .endpoint(agents.defaults.as_deref())
        .ok()
        .flatten();
    let name = match active.config.mode {
        Mode::Environment => agents
            .defaults
            .as_ref()
            .map(|model| model.name())
            .unwrap_or_default(),
        _ => &active.config.name,
    };
    render(AgentPage {
        heading: "Agent 配置",
        section: "agents",
        tab: match query.tab.as_deref() {
            Some("collaboration") => "collaboration",
            _ => "model",
        },
        mode: match active.config.mode {
            Mode::Environment => "environment",
            Mode::Cloud => "cloud",
            Mode::Local => "local",
            Mode::Rules => "rules",
        },
        collaboration: match active.config.collaboration {
            Collaboration::Coordinated => "coordinated",
            Collaboration::StrategyOnly => "strategy_only",
        },
        key_saved: active.config.api_key.as_ref().is_some_and(|saved| {
            endpoint
                .as_ref()
                .is_some_and(|url| url.as_str() == saved.endpoint)
        }) && !active.key_unreadable,
        endpoint: endpoint.map(|url| url.to_string()).unwrap_or_default(),
        name: name.to_owned(),
        configured: active.model.is_some(),
        environment_configured: agents.defaults.is_some(),
        can_save_key: key::CAN_SAVE,
        key_unreadable: active.key_unreadable,
        probe: match query.probe {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        saved: query.saved == Some(1),
    })
}

async fn save(
    State(agents): State<Agents>,
    Form(mut config): Form<Config>,
) -> Result<Redirect, WebError> {
    update_config(&agents, None, |current| {
        config.api_key = current.api_key.take();
        *current = config;
    })
    .await?;
    Ok(Redirect::to("/agents?saved=1"))
}

#[derive(Deserialize)]
struct ModelForm {
    mode: Mode,
    endpoint: String,
    name: String,
    api_key: Option<String>,
}

async fn save_model(
    State(agents): State<Agents>,
    Form(form): Form<ModelForm>,
) -> Result<Redirect, WebError> {
    update_config(&agents, form.api_key, |current| {
        current.mode = form.mode;
        current.endpoint = form.endpoint;
        current.name = form.name;
    })
    .await?;
    Ok(Redirect::to("/agents?saved=1"))
}

#[derive(Deserialize)]
struct CollaborationForm {
    collaboration: Collaboration,
}

async fn save_collaboration(
    State(agents): State<Agents>,
    Form(form): Form<CollaborationForm>,
) -> Result<Redirect, WebError> {
    update_config(&agents, None, |current| {
        current.collaboration = form.collaboration
    })
    .await?;
    Ok(Redirect::to("/agents?tab=collaboration&saved=1"))
}

async fn update_config(
    agents: &Agents,
    api_key: Option<String>,
    update: impl FnOnce(&mut Config),
) -> Result<(), WebError> {
    let _guard = agents.changes.lock().await;
    let mut config = agents
        .active
        .read()
        .expect("agent configuration lock")
        .config
        .clone();
    update(&mut config);
    config.endpoint = config.endpoint.trim().to_owned();
    config.name = config.name.trim().to_owned();
    if config.endpoint.len() > 500 || config.name.len() > 128 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "模型地址或名称超出长度限制。",
        ));
    }
    let invalid = |message| (StatusCode::UNPROCESSABLE_ENTITY, message);
    let url = config
        .endpoint(agents.defaults.as_deref())
        .map_err(invalid)?;
    if let Some(value) = api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if value.len() > key::MAX_LENGTH || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid(
                "API Key 长度最多 1024 字符，且不能包含空格或换行。",
            ));
        }
        let endpoint = url
            .as_ref()
            .ok_or_else(|| invalid("请先配置模型接口。"))?
            .as_str();
        config.api_key = Some(SavedApiKey {
            encrypted: key::protect(value, endpoint).map_err(invalid)?,
            endpoint: endpoint.to_owned(),
        });
    } else if config.api_key.as_ref().is_some_and(|saved| {
        url.as_ref()
            .is_none_or(|url| url.as_str() != saved.endpoint)
    }) {
        config.api_key = None;
    }
    let model = Agents::resolve(&agents.defaults, &config).map_err(invalid)?;
    let persisted = config.clone();
    let store = agents.store.clone();
    database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        settings::write(&tx, "agents", &persisted)?;
        audit::write(
            &tx,
            "设置",
            "Agent 配置修改",
            None,
            "调整了模型来源或协作策略。",
        )?;
        tx.commit()
    })
    .await?;
    *agents.active.write().expect("agent configuration lock") = Active {
        config,
        model,
        key_unreadable: false,
    };
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    ok: bool,
}

async fn probe(State(agents): State<Agents>) -> Result<Redirect, WebError> {
    let Some(model) = agents.current().0 else {
        return Err((StatusCode::CONFLICT, "未配置可测试的模型。"));
    };
    let Some(_slot) = model.permit() else {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "模型执行队列已满，稍后再试。",
        ));
    };
    let prompt = Model::system_prompt("连接测试 Agent", "仅返回 {\"ok\":true}。");
    let reply = model
        .complete_structured::<Probe>(&prompt, &serde_json::json!({"purpose": "connection test"}))
        .await;
    let passed = reply.as_ref().is_ok_and(|reply| reply.ok);
    let detail = match reply {
        Ok(reply) if reply.ok => "模型返回有效的测试响应。".into(),
        Ok(_) => "模型未确认测试请求。".into(),
        Err(error) => error.description(),
    };
    audit::record(&agents.store, "Agent", "连接测试", None, detail).await?;
    Ok(Redirect::to(if passed {
        "/agents?probe=1"
    } else {
        "/agents?probe=0"
    }))
}

pub(crate) fn router(agents: Agents) -> Router {
    Router::new()
        .route(
            "/agents",
            get(page).post(save).layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/agents/model",
            post(save_model).layer(DefaultBodyLimit::max(8192)),
        )
        .route(
            "/agents/collaboration",
            post(save_collaboration).layer(DefaultBodyLimit::max(256)),
        )
        .route("/agents/probe", post(probe))
        .with_state(agents)
}
