use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use askama::Template;
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    response::{Redirect, Response},
    routing::{get, post},
};
use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned, de::IntoDeserializer};

use crate::{
    audit,
    model::{ConnectionObservation, Effort, Model, Protocol},
    settings,
    store::Store,
    web::{WebError, database, render},
};

mod key;

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    #[default]
    Shared,
    Individual,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Strategy,
    Combination,
    Verification,
}

impl Role {
    const ALL: [Self; 3] = [Self::Strategy, Self::Combination, Self::Verification];

    fn id(self) -> &'static str {
        match self {
            Self::Strategy => "strategy",
            Self::Combination => "combination",
            Self::Verification => "verification",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Strategy => "提案策略 Agent",
            Self::Combination => "媒介组合 Agent",
            Self::Verification => "报告核验 Agent",
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Config {
    #[serde(flatten)]
    shared: Profile,
    collaboration: Collaboration,
    #[serde(default)]
    scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    individual: Option<[Profile; 3]>,
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct Profile {
    mode: Mode,
    endpoint: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<SavedApiKey>,
    protocol: Option<Protocol>,
    effort: Effort,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct SavedApiKey {
    endpoint: String,
    encrypted: String,
}

impl Profile {
    fn endpoint(&self, defaults: Option<&Model>) -> Result<Option<Url>, &'static str> {
        match self.mode {
            Mode::Rules => Ok(None),
            Mode::Environment => Ok(defaults.map(|model| model.endpoint().clone())),
            Mode::Cloud | Mode::Local => {
                let url = Url::parse(&self.endpoint).map_err(|_| "请输入有效的模型接口 URL。")?;
                let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
                if (self.mode == Mode::Cloud && url.scheme() != "https")
                    || (self.mode == Mode::Local && !loopback)
                {
                    return Err("云端 API 使用 HTTPS。本地模型使用回环地址。");
                }
                Ok(Some(url))
            }
        }
    }

    fn protocol(&self, defaults: Option<&Model>) -> Protocol {
        if self.mode == Mode::Environment {
            return defaults.map(Model::protocol).unwrap_or_default();
        }
        self.custom_protocol()
    }

    fn custom_protocol(&self) -> Protocol {
        self.protocol
            .or_else(|| {
                Url::parse(&self.endpoint)
                    .ok()
                    .as_ref()
                    .and_then(Protocol::for_endpoint)
            })
            .unwrap_or_default()
    }
}

impl Config {
    fn active_profiles(&self) -> [&Profile; 3] {
        match (&self.scope, &self.individual) {
            (Scope::Individual, Some(profiles)) => profiles.each_ref(),
            _ => [&self.shared; 3],
        }
    }
}

#[derive(Clone, Default)]
struct Resolved {
    model: Option<Arc<Model>>,
    error: Option<&'static str>,
}

#[derive(Clone)]
struct Active {
    config: Config,
    profiles: [Resolved; 3],
    saved_error: Option<&'static str>,
}

#[derive(Clone)]
pub(crate) struct AgentModels {
    profiles: [Option<Arc<Model>>; 3],
}

impl AgentModels {
    pub(crate) fn get(&self, role: Role) -> Option<&Arc<Model>> {
        self.profiles[role as usize].as_ref()
    }

    pub(crate) fn available(&self) -> bool {
        self.profiles.iter().any(Option::is_some)
    }
}

#[derive(Clone)]
pub(crate) struct Agents {
    defaults: Option<Arc<Model>>,
    environment_error: Option<&'static str>,
    active: Arc<RwLock<Active>>,
    changes: Arc<tokio::sync::Mutex<()>>,
    store: Store,
}

impl Agents {
    pub(crate) fn initialize(
        store: Store,
        model: Result<Option<Model>, &'static str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let environment_error = model.as_ref().err().copied();
        let defaults = model.ok().flatten().map(Arc::new);
        let db = store.connect()?;
        let (config, saved_error) = match settings::read::<Config>(&db, "agents") {
            Ok(config) => (config, None),
            Err(rusqlite::Error::FromSqlConversionFailure(..)) => (
                Config {
                    shared: Profile {
                        mode: Mode::Rules,
                        ..Profile::default()
                    },
                    ..Config::default()
                },
                Some("已保存的 Agent 配置格式无效，请重新保存模型配置。"),
            ),
            Err(error) => return Err(error.into()),
        };
        let mut active = Active {
            config,
            profiles: std::array::from_fn(|_| Resolved::default()),
            saved_error,
        };
        let agents = Self {
            defaults,
            environment_error,
            active: Arc::new(RwLock::new(active.clone())),
            changes: Arc::new(tokio::sync::Mutex::new(())),
            store,
        };
        let configured = active.config.active_profiles();
        active.profiles = if active.config.scope == Scope::Shared {
            let shared = agents.resolve_profile(configured[0]);
            [shared.clone(), shared.clone(), shared]
        } else {
            configured.map(|profile| agents.resolve_profile(profile))
        };
        if let Some(error) = active.saved_error {
            audit::write(&db, "Agent", "配置降级", None, error)?;
        }
        for role in Role::ALL {
            if let Some(error) = active.profiles[role as usize].error {
                if active.config.scope == Scope::Shared && role as usize != 0 {
                    break;
                }
                audit::write(
                    &db,
                    "Agent",
                    "配置降级",
                    None,
                    &format!("{} · {error}", role.label()),
                )?;
            }
        }
        *agents.active.write().expect("agent configuration lock") = active;
        Ok(agents)
    }

    fn resolve_profile(&self, profile: &Profile) -> Resolved {
        match self.resolve(profile) {
            Ok(model) => Resolved { model, error: None },
            Err(error) => Resolved {
                model: None,
                error: Some(error),
            },
        }
    }

    fn resolve(&self, profile: &Profile) -> Result<Option<Arc<Model>>, &'static str> {
        if profile.mode == Mode::Environment
            && let Some(error) = self.environment_error
        {
            return Err(error);
        }
        let Some(url) = profile.endpoint(self.defaults.as_deref())? else {
            return Ok(None);
        };
        let saved_key = profile
            .api_key
            .as_ref()
            .filter(|saved| key::CAN_SAVE && saved.endpoint == url.as_str())
            .map(|saved| key::unprotect(&saved.encrypted, url.as_str()))
            .transpose()?;
        if profile.mode == Mode::Environment {
            let model = self.defaults.as_ref().expect("resolved server endpoint");
            if saved_key.is_none() && profile.effort == Effort::Default {
                return Ok(Some(model.clone()));
            }
            return model
                .server_override(saved_key, profile.effort)
                .map(|model| Some(Arc::new(model)));
        }
        let external = profile.mode == Mode::Cloud;
        let key = if saved_key.is_some() {
            saved_key
        } else if external
            && std::env::var("CITYLINE_AGENT_API_KEY_ENDPOINT").as_deref() == Ok(url.as_str())
        {
            std::env::var("CITYLINE_AGENT_API_KEY").ok()
        } else if external
            && self
                .defaults
                .as_ref()
                .is_some_and(|model| model.endpoint() == &url)
        {
            std::env::var("ACCEPTANCE_RUST_MEDIA_MODEL_KEY").ok()
        } else {
            None
        };
        if external && key.as_ref().is_none_or(String::is_empty) {
            return Err("请填写此云端接口的 API Key，或为该地址配置服务端密钥。");
        }
        let protocol = profile.protocol(self.defaults.as_deref());
        Model::connect(
            url,
            profile.name.clone(),
            key,
            external,
            protocol,
            profile.effort,
        )
        .map(|model| Some(Arc::new(model)))
    }

    pub(crate) fn current(&self) -> AgentModels {
        let active = self.active.read().expect("agent configuration lock");
        let collaboration = active.config.collaboration;
        let mut profiles = std::array::from_fn(|index| active.profiles[index].model.clone());
        if matches!(collaboration, Collaboration::StrategyOnly) {
            profiles[Role::Combination as usize] = None;
            profiles[Role::Verification as usize] = None;
        }
        AgentModels { profiles }
    }

    fn profile_view(
        &self,
        id: &'static str,
        label: &'static str,
        profile: &Profile,
        active: Option<&Resolved>,
    ) -> ProfileView {
        let endpoint = profile.endpoint(self.defaults.as_deref()).ok().flatten();
        let error = active.and_then(|entry| entry.error);
        ProfileView {
            id,
            label,
            mode: match profile.mode {
                Mode::Environment | Mode::Rules => "environment",
                Mode::Cloud => "cloud",
                Mode::Local => "local",
            },
            protocol: profile.protocol(self.defaults.as_deref()),
            custom_protocol: profile.custom_protocol(),
            effort: profile.effort,
            endpoint: profile.endpoint.clone(),
            name: profile.name.clone(),
            key_saved: key::CAN_SAVE
                && profile.api_key.as_ref().is_some_and(|saved| {
                    endpoint
                        .as_ref()
                        .is_some_and(|url| url.as_str() == saved.endpoint)
                })
                && error != Some(key::UNREADABLE),
            configured: active.is_some_and(|entry| entry.model.is_some()),
            error,
            connection: active
                .and_then(|entry| entry.model.as_ref())
                .and_then(|model| model.recent_connection()),
        }
    }
}

struct ProfileView {
    id: &'static str,
    label: &'static str,
    mode: &'static str,
    protocol: Protocol,
    custom_protocol: Protocol,
    effort: Effort,
    endpoint: String,
    name: String,
    key_saved: bool,
    configured: bool,
    error: Option<&'static str>,
    connection: Option<ConnectionObservation>,
}

#[derive(Template)]
#[template(path = "agents.html")]
struct AgentPage {
    heading: &'static str,
    section: &'static str,
    tab: &'static str,
    shared: bool,
    profiles: Vec<ProfileView>,
    protocols: [Protocol; 3],
    efforts: [Effort; 4],
    collaboration: &'static str,
    environment_endpoint: String,
    environment_name: String,
    environment_protocol: Protocol,
    can_save_key: bool,
    saved_error: Option<&'static str>,
    environment_error: Option<&'static str>,
    saved: bool,
}

#[derive(Deserialize)]
struct StatusQuery {
    saved: Option<u8>,
    tab: Option<String>,
}

async fn page(State(agents): State<Agents>, Query(query): Query<StatusQuery>) -> Response {
    let active = agents.active.read().expect("agent configuration lock");
    let shared = active.config.scope == Scope::Shared;
    let mut profiles = Vec::with_capacity(4);
    profiles.push(agents.profile_view(
        "shared",
        "统一模型",
        &active.config.shared,
        shared.then_some(&active.profiles[0]),
    ));
    for role in Role::ALL {
        let index = role as usize;
        let profile = active
            .config
            .individual
            .as_ref()
            .map_or(&active.config.shared, |profiles| &profiles[index]);
        profiles.push(agents.profile_view(
            role.id(),
            role.label(),
            profile,
            (!shared).then_some(&active.profiles[index]),
        ));
    }
    let server = agents.defaults.as_deref();
    render(AgentPage {
        heading: "Agent 配置",
        section: "agents",
        tab: if query.tab.as_deref() == Some("collaboration") {
            "collaboration"
        } else {
            "model"
        },
        shared,
        profiles,
        protocols: Protocol::ALL,
        efforts: Effort::ALL,
        collaboration: match active.config.collaboration {
            Collaboration::Coordinated => "coordinated",
            Collaboration::StrategyOnly => "strategy_only",
        },
        can_save_key: key::CAN_SAVE,
        environment_endpoint: server
            .map(|model| model.endpoint().as_str().to_owned())
            .unwrap_or_default(),
        environment_name: server.map(Model::name).unwrap_or_default().to_owned(),
        environment_protocol: server.map(Model::protocol).unwrap_or_default(),
        saved_error: active.saved_error,
        environment_error: agents.environment_error,
        saved: query.saved == Some(1),
    })
}

fn invalid(message: &'static str) -> WebError {
    (StatusCode::UNPROCESSABLE_ENTITY, message)
}

fn parse_value<T: DeserializeOwned>(value: &str) -> Result<T, WebError> {
    T::deserialize(value.into_deserializer())
        .map_err(|_: serde::de::value::Error| invalid("模型配置选项无效。"))
}

fn update_profile(
    agents: &Agents,
    profile: &mut Profile,
    form: &HashMap<String, String>,
    prefix: &str,
) -> Result<(), WebError> {
    let field = |key| {
        form.get(&format!("{prefix}{key}"))
            .map(String::as_str)
            .map(str::trim)
    };
    let mode = parse_value(field("mode").ok_or_else(|| invalid("请选择模型来源。"))?)?;
    if mode == Mode::Rules && !prefix.is_empty() {
        return Err(invalid("请选择服务端配置、云端 API 或本地模型。"));
    }
    profile.mode = mode;
    profile.effort = field("effort")
        .map(parse_value)
        .transpose()?
        .unwrap_or_default();
    if matches!(mode, Mode::Cloud | Mode::Local) {
        profile.endpoint = field("endpoint").unwrap_or_default().to_owned();
        profile.name = field("name").unwrap_or_default().to_owned();
        profile.protocol = field("protocol").map(parse_value).transpose()?;
        if profile.endpoint.len() > 500 || profile.name.len() > 128 {
            return Err(invalid("模型地址或名称超出长度限制。"));
        }
    }
    let endpoint = profile
        .endpoint(agents.defaults.as_deref())
        .map_err(invalid)?;
    if let Some(value) = field("api_key").filter(|value| !value.is_empty()) {
        if value.len() > key::MAX_LENGTH || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid(
                "API Key 长度最多 1024 字符，且不能包含空格或换行。",
            ));
        }
        let endpoint = endpoint
            .as_ref()
            .ok_or_else(|| invalid("请先配置模型接口。"))?
            .as_str();
        profile.api_key = Some(SavedApiKey {
            encrypted: key::protect(value, endpoint).map_err(invalid)?,
            endpoint: endpoint.to_owned(),
        });
    } else if profile.api_key.as_ref().is_some_and(|saved| {
        endpoint
            .as_ref()
            .is_none_or(|url| url.as_str() != saved.endpoint)
    }) {
        profile.api_key = None;
    }
    Ok(())
}

async fn save_model(
    State(agents): State<Agents>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Redirect, WebError> {
    update_config(&agents, true, |config| {
        let scope = form
            .get("scope")
            .map(|value| parse_value(value))
            .transpose()?;
        config.scope = scope.unwrap_or_default();
        if let Some(value) = form.get("collaboration") {
            config.collaboration = parse_value(value)?;
        }
        match config.scope {
            Scope::Shared => update_profile(
                &agents,
                &mut config.shared,
                &form,
                if scope.is_some() { "shared_" } else { "" },
            )?,
            Scope::Individual => {
                let profiles = config
                    .individual
                    .get_or_insert_with(|| std::array::from_fn(|_| config.shared.clone()));
                for role in Role::ALL {
                    update_profile(
                        &agents,
                        &mut profiles[role as usize],
                        &form,
                        &format!("{}_", role.id()),
                    )?;
                }
            }
        }
        Ok(())
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
    update_config(&agents, false, |config| {
        config.collaboration = form.collaboration;
        Ok(())
    })
    .await?;
    Ok(Redirect::to("/agents?tab=collaboration&saved=1"))
}

async fn update_config(
    agents: &Agents,
    models_edited: bool,
    update: impl FnOnce(&mut Config) -> Result<(), WebError>,
) -> Result<(), WebError> {
    let _guard = agents.changes.lock().await;
    let mut active = agents
        .active
        .read()
        .expect("agent configuration lock")
        .clone();
    let previous = active.config.clone();
    update(&mut active.config)?;
    if models_edited {
        let old_profiles = previous.active_profiles();
        let new_profiles = active.config.active_profiles();
        let resolve = |index: usize| {
            let profile = new_profiles[index];
            let entry = if previous.scope == active.config.scope
                && profile == old_profiles[index]
                && active.profiles[index].error.is_none()
            {
                active.profiles[index].clone()
            } else {
                agents.resolve_profile(profile)
            };
            if let Some(error) = entry.error
                && profile.mode != Mode::Environment
            {
                return Err(invalid(error));
            }
            Ok(entry)
        };
        active.profiles = if active.config.scope == Scope::Shared {
            let shared = resolve(0)?;
            [shared.clone(), shared.clone(), shared]
        } else {
            [resolve(0)?, resolve(1)?, resolve(2)?]
        };
        active.saved_error = None;
    }
    let persisted = active.config.clone();
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
            "调整了模型接入或协作策略。",
        )?;
        tx.commit()
    })
    .await?;
    *agents.active.write().expect("agent configuration lock") = active;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    ok: bool,
}

#[derive(Deserialize)]
struct ProbeQuery {
    role: Option<Role>,
}

async fn probe(
    State(agents): State<Agents>,
    Query(query): Query<ProbeQuery>,
) -> Result<Redirect, WebError> {
    let model = {
        let active = agents.active.read().expect("agent configuration lock");
        let role = query.role.unwrap_or(Role::Strategy);
        active.profiles[role as usize].model.clone()
    };
    let Some(model) = model else {
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
    model.observe_connection("手动测试", passed, detail.clone());
    let label = query.role.map(Role::label).unwrap_or("统一模型");
    audit::record(
        &agents.store,
        "Agent",
        "连接测试",
        None,
        format!("{label} · {detail}"),
    )
    .await?;
    Ok(Redirect::to("/agents"))
}

pub(crate) fn router(agents: Agents) -> Router {
    Router::new()
        .route(
            "/agents",
            get(page)
                .post(save_model)
                .layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/agents/model",
            post(save_model).layer(DefaultBodyLimit::max(24_576)),
        )
        .route(
            "/agents/collaboration",
            post(save_collaboration).layer(DefaultBodyLimit::max(256)),
        )
        .route("/agents/probe", post(probe))
        .with_state(agents)
}
