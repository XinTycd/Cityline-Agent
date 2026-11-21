use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{process::Command, sync::Semaphore};
use uuid::Uuid;

use crate::{
    auth::User,
    media::{
        optimizer::{Policy, Portfolio},
        platform::{EvidencePlan, EvidenceRefresh},
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
    pub(crate) policy: Policy,
    #[serde(skip)]
    pub(crate) portfolio: Arc<OnceLock<Portfolio>>,
    #[serde(skip)]
    pub(crate) evidence_plan: Arc<OnceLock<Vec<EvidencePlan>>>,
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
    #[serde(default)]
    pub(crate) newcloud: bool,
    #[serde(default)]
    pub(crate) redbean: bool,
    #[serde(default, rename = "redbeanFallback")]
    pub(crate) redbean_fallback: bool,
    #[serde(default)]
    pub(crate) period: Option<String>,
    #[serde(default)]
    pub(crate) inventory: InventoryReads,
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
                period: None,
                evidence: None,
                inventory: InventoryReads::default(),
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
    pub(crate) maps: usize,
    pub(crate) expected_maps: usize,
    pub(crate) reach: usize,
    pub(crate) expected_reach: usize,
    #[serde(default)]
    pub(crate) observed_reach: usize,
}

impl Refresh {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            port,
            directory: ".operations-data/browser-refresh".into(),
            slot: Arc::new(Semaphore::new(1)),
            active: Arc::new(Mutex::new(None)),
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

    pub(crate) fn authorized(&self, headers: &HeaderMap, verifying: bool) -> Option<BrowserImport> {
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
        let expected = if verifying { "proof" } else { "importing" };
        let permitted = if job.request.proof.is_some() {
            verifying
        } else {
            state.redbean
        };
        (state.state == expected && state.newcloud && permitted).then_some(job)
    }

    pub(crate) fn launch(&self, request: BrowserRequest, actor: &str) -> Result<Uuid, WebError> {
        let tariff = if request.proof.is_some() {
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
            || (request.proof.is_none()
                && provider_config::origin("CITYLINE_REDBEAN_ORIGIN").is_none())
        {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "请在本机环境配置中填写新云和红豆平台的加密服务地址，再启动工作台。",
            ));
        }
        if !Path::new("node_modules/playwright-core/package.json").is_file() {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "本机尚未安装取数组件。"));
        }
        let permit = self
            .slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| (StatusCode::CONFLICT, "已有取数任务运行，请等待完成。"))?;
        let id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let directory = self.directory.join(id.to_string());
        std::fs::create_dir_all(&directory)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法创建本机取数目录。"))?;
        let status = directory.join("status.json");
        let settings = serde_json::to_vec(&request)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法准备取数参数。"))?;
        std::fs::write(directory.join("request.json"), settings)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法写入取数参数。"))?;
        let mut command = Command::new("node");
        if request.proof.is_some() {
            command
                .env_remove("CITYLINE_REDBEAN_ACCESS_TOKEN")
                .env_remove("CITYLINE_REDBEAN_TENANT_ID");
        } else if let Some((token, tenant)) = self.redbean.token_for_task(actor) {
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
            .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "本机取数进程无法启动。"))?;
        let deadline = 620 + 150 * request.periods.len().saturating_sub(1) as u64;
        *self.active.lock().expect("refresh lock") = Some(ActiveImport {
            token,
            job: BrowserImport { id, request },
        });
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
            return Err((StatusCode::NOT_FOUND, "未找到本机取数任务。"));
        }
        Ok(BrowserStatus::read(&directory.join("status.json")))
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
            policy: Policy::default(),
            portfolio: Arc::default(),
            evidence_plan: Arc::default(),
            proof: None,
        },
        &user.identifier,
    )?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

async fn progress(
    State(app): State<Refresh>,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<BrowserStatus>, WebError> {
    Ok(Json(app.status(id)?))
}

pub(crate) fn router(app: Refresh) -> Router {
    Router::new()
        .route("/api/media-refresh", post(start))
        .route("/api/media-refresh/{id}", get(progress))
        .with_state(app)
}
mod redbean;
