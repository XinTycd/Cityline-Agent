use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Semaphore, watch},
};
use uuid::Uuid;

pub(crate) use inventory::{InventoryCheck, InventoryFailure, InventoryIssue, StockDemand};

use crate::{
    auth::User,
    media::{
        optimizer::{Policy, Portfolio},
        platform::{EvidenceManifest, EvidenceRefresh, ReachCheck},
        schedule::Rolling,
    },
    provider_config,
    web::WebError,
};

#[derive(Clone)]
pub(crate) struct Refresh {
    port: u16,
    directory: PathBuf,
    slot: Arc<Semaphore>,
    active: Arc<Mutex<Option<ActiveImport>>>,
    newcloud: Arc<Mutex<newcloud::Sessions>>,
    pub(crate) redbean: redbean::Redbean,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRequest {
    pub(crate) industry: &'static str,
    pub(crate) center: String,
    pub(crate) starts: String,
    pub(crate) ends: String,
    pub(crate) media: &'static str,
    pub(crate) radius: u16,
    pub(crate) supplemental_center: Option<&'static str>,
    pub(crate) required_routes: Vec<String>,
    pub(crate) periods: Vec<BrowserWindow>,
    pub(crate) budget: u32,
    pub(crate) rolling: Option<Rolling>,
    #[serde(skip)]
    pub(crate) target_centers: Vec<String>,
    #[serde(skip)]
    pub(crate) core_circle: Option<String>,
    #[serde(skip)]
    pub(crate) policy: watch::Receiver<Option<Policy>>,
    #[serde(skip)]
    pub(crate) portfolio: Arc<OnceLock<Portfolio>>,
    #[serde(skip)]
    pub(crate) evidence_plan: Arc<OnceLock<EvidenceManifest>>,
    #[serde(skip)]
    pub(crate) saved_overview_routes: Vec<BTreeSet<String>>,
    #[serde(rename = "proofOnly", serialize_with = "serialize_proof_mode")]
    pub(crate) proof: Option<Arc<EvidenceRefresh>>,
}

fn serialize_proof_mode<S: serde::Serializer>(
    proof: &Option<Arc<EvidenceRefresh>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_bool(proof.is_some())
}

#[derive(Clone, Serialize)]
pub(crate) struct BrowserWindow {
    pub(crate) starts: String,
    pub(crate) ends: String,
}

impl BrowserWindow {
    pub(crate) fn for_schedule(
        starts: &str,
        ends: &str,
        rolling: Option<&Rolling>,
    ) -> Result<Vec<Self>, WebError> {
        let Some(rolling) = rolling else {
            return Ok(vec![Self {
                starts: starts.into(),
                ends: ends.into(),
            }]);
        };
        let periods: Vec<_> = rolling
            .windows(starts, ends)
            .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?
            .into_iter()
            .filter(|window| window.needs_stock())
            .map(|window| Self {
                starts: window.starts,
                ends: window.ends,
            })
            .collect();
        if periods.is_empty() {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "请至少安排一段采购档期。"));
        }
        Ok(periods)
    }
}

struct ActiveImport {
    token: Uuid,
    job: BrowserImport,
    newcloud_scope: Option<Uuid>,
    reused_newcloud: bool,
}

pub(crate) const NEWCLOUD_SESSION_PATH: &str = "/api/media-refresh/newcloud-session";

pub(crate) enum WorkerAccess {
    Inventory,
    Evidence,
    NewcloudSession,
}

#[derive(Clone)]
pub(crate) struct BrowserImport {
    pub(crate) id: Uuid,
    pub(crate) request: BrowserRequest,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct BrowserStatus {
    pub(crate) state: String,
    pub(crate) report: Option<String>,
    #[serde(default)]
    pub(crate) evidence: Option<EvidenceReport>,
    #[serde(default, rename = "reachChecks", skip_serializing_if = "Vec::is_empty")]
    pub(crate) reach_checks: Vec<ReachCheck>,
    #[serde(default)]
    pub(crate) newcloud: bool,
    #[serde(default)]
    pub(crate) redbean: bool,
    #[serde(default, rename = "redbeanFallback")]
    pub(crate) redbean_fallback: bool,
    #[serde(default, rename = "newcloudFallback")]
    pub(crate) newcloud_fallback: bool,
    #[serde(default, rename = "newcloudReused")]
    pub(crate) newcloud_reused: bool,
    #[serde(default)]
    pub(crate) period: Option<String>,
    #[serde(default)]
    pub(crate) inventory: InventoryReads,
    #[serde(default)]
    pub(crate) imported: usize,
}

impl BrowserStatus {
    fn read(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|content| serde_json::from_slice(&content).ok())
            .unwrap_or_else(|| Self {
                state: "opening".into(),
                report: None,
                newcloud: false,
                redbean: false,
                redbean_fallback: false,
                newcloud_fallback: false,
                newcloud_reused: false,
                period: None,
                evidence: None,
                reach_checks: Vec::new(),
                inventory: InventoryReads::default(),
                imported: 0,
            })
    }

    pub(crate) fn terminal(&self) -> bool {
        matches!(self.state.as_str(), "done" | "needs_review") || self.state.starts_with("error_")
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct InventoryReads {
    pub(crate) api: u32,
    pub(crate) exports: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvidenceReport {
    pub(crate) reach: usize,
    pub(crate) expected_reach: usize,
    #[serde(default)]
    pub(crate) observed_reach: usize,
    #[serde(default)]
    pub(crate) baseline_reach: usize,
    #[serde(default)]
    pub(crate) exits: usize,
    #[serde(default)]
    pub(crate) expected_exits: usize,
    #[serde(default)]
    pub(crate) overview_maps: usize,
    #[serde(default)]
    pub(crate) expected_overview_maps: usize,
}

impl Refresh {
    pub(crate) fn new(port: u16, directory: PathBuf) -> Self {
        Self {
            port,
            directory,
            slot: Arc::new(Semaphore::new(1)),
            active: Arc::new(Mutex::new(None)),
            newcloud: Arc::default(),
            redbean: redbean::Redbean::from_environment(),
        }
    }

    pub(crate) fn is_active(&self, id: Uuid) -> bool {
        self.active
            .lock()
            .expect("refresh lock")
            .as_ref()
            .is_some_and(|active| active.job.id == id)
    }

    pub(crate) async fn verify_redbean_inventory(
        &self,
        actor: &str,
        checks: &[InventoryCheck],
    ) -> Result<(), InventoryFailure> {
        use std::{env, process::Stdio};
        const MAX_FINDINGS_BYTES: usize = 1024 * 1024;
        const UNAVAILABLE: WebError = (
            StatusCode::SERVICE_UNAVAILABLE,
            "暂时无法完成红豆库存复核，请核对平台连接后重试。",
        );
        const AUTHORIZATION: WebError = (
            StatusCode::CONFLICT,
            "请在系统设置中完成红豆授权，再确认所选方案。",
        );
        let (token, tenant) = self
            .redbean
            .token_for_task(actor)
            .or_else(|| {
                let token = env::var("CITYLINE_REDBEAN_ACCESS_TOKEN").ok()?;
                let tenant = env::var("CITYLINE_REDBEAN_TENANT_ID").unwrap_or_else(|_| "1".into());
                (!token.is_empty()).then_some((token, tenant))
            })
            .ok_or(AUTHORIZATION)?;
        let _permit = self.slot.try_acquire().map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "另有获取数据或库存复核任务正在执行，请稍后再确认。",
            )
        })?;
        let payload = serde_json::to_vec(checks).map_err(|_| UNAVAILABLE)?;
        let mut command = Command::new("node");
        command
            .env_clear()
            .arg("scripts/recheck-media-inventory.mjs")
            .env("CITYLINE_REDBEAN_ACCESS_TOKEN", &token)
            .env("CITYLINE_REDBEAN_TENANT_ID", tenant)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for variable in [
            "PATH",
            "SystemRoot",
            "TEMP",
            "TMP",
            "NODE_EXTRA_CA_CERTS",
            "CITYLINE_REDBEAN_ORIGIN",
            "CITYLINE_REDBEAN_LOGIN",
            "CITYLINE_REDBEAN_API_PREFIX",
            "CITYLINE_REDBEAN_DICTIONARY",
            "CITYLINE_REDBEAN_BUS_PAGE",
            "CITYLINE_REDBEAN_METRO_PAGE",
        ] {
            if let Some(value) = env::var_os(variable) {
                command.env(variable, value);
            }
        }
        let mut child = command.spawn().map_err(|_| UNAVAILABLE)?;
        let mut input = child.stdin.take().ok_or(UNAVAILABLE)?;
        let stdout = child.stdout.take().ok_or(UNAVAILABLE)?;
        let read = async move {
            let mut output = Vec::new();
            stdout
                .take((MAX_FINDINGS_BYTES + 1) as u64)
                .read_to_end(&mut output)
                .await?;
            Ok::<_, std::io::Error>(output)
        };
        let (outcome, output) = tokio::time::timeout(Duration::from_secs(120), async {
            input.write_all(&payload).await?;
            drop(input);
            tokio::try_join!(child.wait(), read)
        })
        .await
        .map_err(|_| UNAVAILABLE)?
        .map_err(|_| UNAVAILABLE)?;
        match outcome.code() {
            Some(0) if output.is_empty() => Ok(()),
            Some(2) if output.len() <= MAX_FINDINGS_BYTES => {
                Err(InventoryFailure::from_findings(&output, checks)
                    .unwrap_or_else(|| UNAVAILABLE.into()))
            }
            Some(3) => {
                self.redbean.clear_rejected(actor, &token);
                Err(AUTHORIZATION.into())
            }
            _ => Err(UNAVAILABLE.into()),
        }
    }

    pub(crate) fn authorized(
        &self,
        headers: &HeaderMap,
        access: WorkerAccess,
    ) -> Option<BrowserImport> {
        let token = headers
            .get("x-cityline-media-import")?
            .to_str()
            .ok()?
            .parse::<Uuid>()
            .ok()?;
        let job = self
            .active
            .lock()
            .expect("refresh lock")
            .as_ref()
            .filter(|active| active.token == token)?
            .job
            .clone();
        let state = self.status(job.id).ok()?;
        let permitted = match access {
            WorkerAccess::Inventory => {
                state.state == "importing"
                    && job.request.proof.is_none()
                    && state.newcloud
                    && state.redbean
            }
            WorkerAccess::Evidence => {
                state.state == "proof"
                    && state.newcloud
                    && (job.request.proof.is_some() || state.redbean)
            }
            WorkerAccess::NewcloudSession => state.state == "login",
        };
        permitted.then_some(job)
    }

    pub(crate) fn clear_newcloud(&self, scope: Uuid) {
        let mut active = self.active.lock().expect("refresh lock");
        if let Some(job) = active
            .as_mut()
            .filter(|worker| worker.newcloud_scope == Some(scope))
        {
            job.newcloud_scope = None;
        }
        self.newcloud
            .lock()
            .expect("newcloud session lock")
            .clear(scope);
    }

    pub(crate) fn preflight(proof_only: bool, media: &str) -> Result<PathBuf, WebError> {
        let tariff = if proof_only {
            PathBuf::new()
        } else {
            std::env::var_os("CITYLINE_CONTEST_TARIFF")
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .ok_or((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "本机尚未设置赛题刊例路径。",
                ))?
        };
        if provider_config::origin("CITYLINE_NEWCLOUD_ORIGIN").is_none()
            || (!proof_only && provider_config::origin("CITYLINE_REDBEAN_ORIGIN").is_none())
        {
            let notice = if proof_only {
                "请在本机环境配置中填写新云平台的加密服务地址，再启动工作台。"
            } else {
                "请在本机环境配置中填写新云和红豆平台的加密服务地址，再启动工作台。"
            };
            return Err((StatusCode::SERVICE_UNAVAILABLE, notice));
        }
        let paths_ready = |fields: &[&str]| {
            fields
                .iter()
                .all(|name| provider_config::path(name).is_some())
        };
        let existing_cloud_session = std::env::var("CITYLINE_NEWCLOUD_SESSION")
            .is_ok_and(|session| !session.trim().is_empty());
        let interactive_cloud_login =
            paths_ready(&["CITYLINE_NEWCLOUD_LOGIN", "CITYLINE_NEWCLOUD_LANDING"])
                && std::env::var("CITYLINE_NEWCLOUD_READY_MARKER")
                    .is_ok_and(|marker| !marker.trim().is_empty());
        if !(existing_cloud_session || interactive_cloud_login)
            || !paths_ready(&["CITYLINE_NEWCLOUD_COVERAGE_PAGE"])
            || (!proof_only && !paths_ready(&["CITYLINE_NEWCLOUD_COVERAGE"]))
        {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "新云平台登录或覆盖查询配置不完整，请核对本机环境配置。",
            ));
        }
        if !proof_only
            && (!paths_ready(&["CITYLINE_REDBEAN_LOGIN", "CITYLINE_REDBEAN_API_PREFIX"])
                || (media != "metro" && !paths_ready(&["CITYLINE_REDBEAN_BUS_PAGE"]))
                || (media != "bus"
                    && !paths_ready(&["CITYLINE_REDBEAN_METRO_PAGE", "CITYLINE_REDBEAN_SPECS"])))
        {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "红豆平台登录、库存查询或地铁规格配置不完整，请核对本机环境配置。",
            ));
        }
        if !Path::new("node_modules/playwright-core/package.json").is_file() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "本机尚未安装获取数据所需的组件。",
            ));
        }
        Ok(tariff)
    }

    pub(crate) fn launch(&self, request: BrowserRequest, actor: &User) -> Result<Uuid, WebError> {
        let tariff = Self::preflight(request.proof.is_some(), request.media)?;
        let permit = self.slot.clone().try_acquire_owned().map_err(|_| {
            (
                StatusCode::CONFLICT,
                "已有获取数据的任务正在运行，请等待完成。",
            )
        })?;
        let id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let directory = self.directory.join(id.to_string());
        std::fs::create_dir_all(&directory).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法创建本机的数据获取目录。",
            )
        })?;
        let status = directory.join("status.json");
        let settings = serde_json::to_vec(&request).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法准备获取数据所需的参数。",
            )
        })?;
        std::fs::write(directory.join("request.json"), settings).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法写入获取数据所需的参数。",
            )
        })?;
        let mut active = self.active.lock().expect("refresh lock");
        let mut command = Command::new("node");
        command.env_remove("CITYLINE_NEWCLOUD_REUSED");
        let cached = self
            .newcloud
            .lock()
            .expect("newcloud session lock")
            .get(actor.platform_scope);
        let reused_newcloud = cached.is_some();
        if let Some(session) = cached {
            command
                .env("CITYLINE_NEWCLOUD_SESSION", session)
                .env("CITYLINE_NEWCLOUD_REUSED", "1");
        }
        if request.proof.is_some() {
            command
                .env_remove("CITYLINE_REDBEAN_ACCESS_TOKEN")
                .env_remove("CITYLINE_REDBEAN_TENANT_ID");
        } else if let Some((token, tenant)) = self.redbean.token_for_task(&actor.identifier) {
            command
                .env("CITYLINE_REDBEAN_ACCESS_TOKEN", token)
                .env("CITYLINE_REDBEAN_TENANT_ID", tenant);
        }
        let child = command
            .arg("scripts/refresh-media-browser.mjs")
            .arg(self.port.to_string())
            .arg(&directory)
            .arg(tariff)
            .env_remove("ACCEPTANCE_RUST_MEDIA_MODEL_KEY")
            .env_remove("CITYLINE_AGENT_API_KEY")
            .env_remove("CITYLINE_FEISHU_APP_SECRET")
            .env_remove("CITYLINE_OIDC_CLIENT_SECRET")
            .env_remove("CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET")
            .env_remove("CITYLINE_REDBEAN_OAUTH_CLIENT_ID")
            .env("CITYLINE_MEDIA_IMPORT_TOKEN", token.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "本机无法启动获取数据的进程。",
                )
            })?;
        let deadline = 620 + 150 * request.periods.len().saturating_sub(1) as u64;
        *active = Some(ActiveImport {
            token,
            job: BrowserImport { id, request },
            newcloud_scope: Some(actor.platform_scope),
            reused_newcloud,
        });
        drop(active);
        let active = self.active.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut child = child;
            let outcome = tokio::time::timeout(Duration::from_secs(deadline), child.wait()).await;
            let timed_out = outcome.is_err();
            if timed_out {
                let _ = child.kill().await;
            }
            let mut last = BrowserStatus::read(&status);
            if !last.terminal() {
                last.state = if timed_out {
                    "error_timeout"
                } else if last.state == "opening" {
                    "error_launch"
                } else {
                    "error_interrupted"
                }
                .into();
                if let Ok(payload) = serde_json::to_vec(&last) {
                    let pending = status.with_extension("terminal.tmp");
                    if std::fs::write(&pending, payload).is_ok() {
                        let _ = std::fs::rename(pending, &status);
                    }
                }
            }
            *active.lock().expect("refresh lock") = None;
        });
        Ok(id)
    }

    pub(crate) fn status(&self, id: Uuid) -> Result<BrowserStatus, WebError> {
        let directory = self.directory.join(id.to_string());
        if !directory.is_dir() {
            return Err((StatusCode::NOT_FOUND, "未找到本机获取数据的任务。"));
        }
        Ok(BrowserStatus::read(&directory.join("status.json")))
    }

    pub(crate) fn route_overview_archive(&self, id: Uuid) -> Option<Vec<u8>> {
        let path = self.directory.join(id.to_string()).join("annual-maps.json");
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(file, 2 * 1024 * 1024 + 1),
            &mut bytes,
        )
        .ok()?;
        (bytes.len() <= 2 * 1024 * 1024).then_some(bytes)
    }
}

async fn start(
    State(app): State<Refresh>,
    Extension(user): Extension<User>,
) -> Result<(StatusCode, Json<Value>), WebError> {
    let (starts, ends) = (crate::media::SHOWCASE_START, crate::media::SHOWCASE_END);
    let rolling = Rolling::showcase("", starts, ends).expect("default showcase phases are valid");
    let periods = BrowserWindow::for_schedule(starts, ends, Some(&rolling))?;
    let id = app.launch(
        BrowserRequest {
            industry: "food",
            center: crate::media::FOOD_COVERAGE_CENTERS.into(),
            starts: starts.into(),
            ends: ends.into(),
            media: "both",
            radius: crate::media::FOOD_COVERAGE_RADIUS,
            supplemental_center: None,
            required_routes: Vec::new(),
            periods,
            budget: crate::media::SHOWCASE_BUDGET,
            rolling: Some(rolling),
            target_centers: Vec::new(),
            core_circle: None,
            policy: watch::channel(Some(Policy::default())).1,
            portfolio: Arc::default(),
            evidence_plan: Arc::default(),
            saved_overview_routes: Vec::new(),
            proof: None,
        },
        &user,
    )?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

async fn progress(
    State(app): State<Refresh>,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<BrowserStatus>, WebError> {
    let mut progress = app.status(id)?;
    progress.reach_checks.clear();
    Ok(Json(progress))
}

async fn newcloud_session(
    State(app): State<Refresh>,
    worker: Option<Extension<BrowserImport>>,
    Json(update): Json<newcloud::Update>,
) -> Result<StatusCode, WebError> {
    const FORBIDDEN: WebError = (StatusCode::FORBIDDEN, "当前工作台会话未授权缓存新云连接。");
    let Extension(worker) = worker.ok_or(FORBIDDEN)?;
    let active = app.active.lock().expect("refresh lock");
    let active = active
        .as_ref()
        .filter(|active| active.job.id == worker.id)
        .ok_or(FORBIDDEN)?;
    let scope = active.newcloud_scope.ok_or(FORBIDDEN)?;
    let status = app.status(worker.id)?;
    if status.state != "login" {
        return Err(FORBIDDEN);
    }
    let mut sessions = app.newcloud.lock().expect("newcloud session lock");
    match update.session {
        Some(session) if status.newcloud => {
            if !sessions.remember(scope, session, update.max_age) {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "新云会话格式或有效期不可用。",
                ));
            }
        }
        None if active.reused_newcloud => sessions.clear(scope),
        _ => return Err(FORBIDDEN),
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn router(app: Refresh) -> Router {
    Router::new()
        .route("/api/media-refresh", post(start))
        .route("/api/media-refresh/{id}", get(progress))
        .route(
            NEWCLOUD_SESSION_PATH,
            post(newcloud_session).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .with_state(app)
}
mod inventory;
mod newcloud;
mod redbean;
