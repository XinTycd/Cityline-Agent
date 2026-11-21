use std::{
    io::{Cursor, Read},
    sync::LazyLock,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart},
    http::StatusCode,
    routing::post,
};
use quick_xml::{Reader, events::Event};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{Industry, Options, Selection, allowed_budget, full_month, month_days, period_months};
use crate::{store::Store, web::WebError};

const BRIEF_MAX: usize = 180 * 1024;
const INVALID: WebError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "请粘贴 Brief 邮件正文，或上传不超过 180 KiB 的 UTF-8 文本或 DOCX。",
);

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct BriefContext {
    #[serde(default)]
    pub(super) customer_profile: String,
    pub(super) objective: String,
    #[serde(default)]
    pub(super) campaign_goal: String,
    #[serde(default)]
    pub(super) audience: String,
    pub(super) region: String,
    pub(super) monitoring: String,
}

impl BriefContext {
    pub(super) fn is_empty(&self) -> bool {
        [
            &self.customer_profile,
            &self.objective,
            &self.campaign_goal,
            &self.audience,
            &self.region,
            &self.monitoring,
        ]
        .into_iter()
        .all(|field| field.is_empty())
    }

    pub(super) fn normalize(&mut self) -> bool {
        [
            &mut self.customer_profile,
            &mut self.objective,
            &mut self.campaign_goal,
            &mut self.audience,
            &mut self.region,
            &mut self.monitoring,
        ]
        .into_iter()
        .all(|field| {
            *field = super::anonymize_food_case(field.trim()).into_owned();
            field.chars().count() <= 150 && !field.chars().any(char::is_control)
        })
    }
}

pub(super) fn campaign_goal(value: &str) -> Option<&str> {
    let goal = value.trim();
    matches!(goal, "到店" | "认知" | "促销" | "拉新").then_some(goal)
}

static EMPTY_BRIEF: LazyLock<BriefContext> = LazyLock::new(BriefContext::default);

pub(super) struct CampaignView<'a> {
    pub(super) brief: &'a BriefContext,
    pub(super) starts: &'a str,
    pub(super) ends: &'a str,
    pub(super) budget: u32,
    pub(super) media: Selection,
    pub(super) target_centers: &'a [String],
}

impl<'a> CampaignView<'a> {
    pub(super) fn new(options: &'a Options) -> Self {
        Self {
            brief: options.brief.as_ref().unwrap_or(&EMPTY_BRIEF),
            starts: options.starts.as_deref().unwrap_or("待确认"),
            ends: options.ends.as_deref().unwrap_or("待确认"),
            budget: options.budget.unwrap_or(super::SHOWCASE_BUDGET),
            media: options.media.unwrap_or(Selection::Both),
            target_centers: &options.target_centers,
        }
    }
}

#[derive(Serialize)]
struct Extraction {
    subject: String,
    industry: Option<&'static str>,
    budget: Option<u32>,
    budget_cap_wan: Option<u32>,
    month: Option<String>,
    starts: Option<String>,
    ends: Option<String>,
    bus: bool,
    metro: bool,
    required_routes: Vec<String>,
    outside_theme: bool,
    brief: BriefContext,
    issues: Vec<&'static str>,
}

static BUDGET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<amount>[0-9]+(?:[.．][0-9]+)?)\s*(?:万(?:元)?|人民币万)").unwrap()
});
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(20[0-9]{2})\s*(?:年|[-/.])\s*([0-9]{1,2})(?:\s*(?:月|[-/.])\s*([0-9]{1,2})\s*日?)?",
    )
    .unwrap()
});
static TAIL_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:至|到|—|–|~|～|\s-\s)\s*([0-9]{1,2})\s*月\s*([0-9]{1,2})\s*日?").unwrap()
});

fn field<'a>(lines: &[&'a str], labels: &[&str]) -> Option<&'a str> {
    lines.iter().find_map(|line| {
        let (key, content) = line.split_once(['：', ':'])?;
        let key = key.trim_matches(|ch: char| {
            ch.is_whitespace() || ch.is_ascii_digit() || matches!(ch, '.' | '、' | '．')
        });
        labels.contains(&key).then(|| content.trim())
    })
}

fn excerpt(value: &str, maximum: usize) -> String {
    super::anonymize_food_case(value)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(maximum)
        .collect()
}

fn month(period: &str) -> Option<String> {
    if matches!(period.trim(), "2027 全年" | "2027全年" | "2027年全年") {
        return Some(super::SHOWCASE_DATES.into());
    }
    let mut dates = DATE.captures_iter(period);
    let first = dates.next()?;
    let year: u32 = first[1].parse().ok()?;
    let month: u8 = first[2].parse().ok()?;
    let value = format!("{year:04}-{month:02}");
    if year != 2027 {
        return None;
    }
    month_days(&value[5..])?;
    if let Some(start_day) = first.get(3) {
        let start = format!("{value}-{:02}", start_day.as_str().parse::<u8>().ok()?);
        let end = if let Some(last) = dates.next() {
            format!(
                "{:04}-{:02}-{:02}",
                last[1].parse::<u32>().ok()?,
                last[2].parse::<u8>().ok()?,
                last.get(3)?.as_str().parse::<u8>().ok()?
            )
        } else {
            let tail = TAIL_DATE.captures(period)?;
            format!(
                "{year:04}-{:02}-{:02}",
                tail[1].parse::<u8>().ok()?,
                tail[2].parse::<u8>().ok()?
            )
        };
        if dates.next().is_some() || period_months(&start, &end).is_none() {
            return None;
        }
        if !full_month(&start, &end) {
            return Some(format!("{start} 至 {end}"));
        }
    } else if dates.next().is_some()
        || period.contains("–")
        || period.contains("至")
        || period.contains("天")
    {
        return None;
    }
    Some(value)
}

fn extract(mail: &str) -> Extraction {
    let lines = mail.lines().map(str::trim).collect::<Vec<_>>();
    let subject = excerpt(field(&lines, &["主题", "Subject"]).unwrap_or_default(), 150);
    let mut issues = Vec::new();
    let industry = field(&lines, &["行业", "品类", "客户行业"])
        .and_then(Industry::from_text)
        .map(Industry::label);
    let outside_theme = industry.is_some_and(|name| name != Industry::Food.label());
    if outside_theme {
        issues.push("邮件指定的行业与本场食品饮料主题不同。");
    } else if industry.is_none() {
        issues.push("行业未明确，请核实邮件是否对应食品饮料主题。");
    }
    let amount = |value: &str| {
        let value = value.replace([',', '，'], "");
        let mut amounts = BUDGET.captures_iter(&value);
        let amount = amounts
            .next()
            .and_then(|first| first["amount"].parse::<u32>().ok())?;
        amounts.next().is_none().then_some(amount)
    };
    let budget_text = field(&lines, &["预算", "投放预算", "核定预算"]);
    let is_cap = |value: &str| {
        ["不超过", "不得超过", "最高", "上限"]
            .iter()
            .any(|key| value.contains(key))
    };
    let budget = budget_text
        .filter(|value| !is_cap(value))
        .and_then(amount)
        .filter(|amount| allowed_budget(*amount));
    let cap_text = field(&lines, &["预算上限", "预算硬上限", "最高预算"])
        .or_else(|| budget_text.filter(|value| is_cap(value)));
    let budget_cap_wan = cap_text
        .and_then(amount)
        .filter(|amount| (1..=super::MAX_EXPLICIT_CAP / 10_000).contains(&u64::from(*amount)));
    if cap_text.is_some() && budget_cap_wan.is_none() {
        issues.push("邮件中的预算硬上限需要人工核对，请填写正整数万元。");
    }
    if budget.is_none() && budget_cap_wan.is_none() {
        issues.push("缺少符合赛题的明确预算，请填写 100、200 或 500 万元。");
    }
    let media = field(&lines, &["媒体", "媒体形式", "投放媒体", "投放形式"]).unwrap_or_default();
    let bus = ["公交车身", "公交", "巴士"]
        .iter()
        .any(|keyword| media.contains(keyword));
    let metro = ["地铁出入口", "地铁出口", "地铁入口"]
        .iter()
        .any(|keyword| media.contains(keyword));
    let outside_media = ["LED", "大牌", "户外屏", "地铁灯箱"]
        .iter()
        .any(|keyword| media.to_uppercase().contains(keyword));
    if outside_media {
        issues.push("邮件包含赛题范围外的媒体，请核对公交车身或地铁出入口。");
    }
    let month = field(
        &lines,
        &["投放档期", "投放周期", "投放时间", "投放月份", "排期"],
    )
    .and_then(month);
    if month.is_none() {
        issues.push("档期需在 2027 年内，并提供月份或有效起止日期。");
    }
    let monitor = lines
        .iter()
        .copied()
        .filter(|line| line.contains("上刊照片") || line.contains("下刊照片"))
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let brief = BriefContext {
        customer_profile: excerpt(
            field(&lines, &["脱敏客户画像", "脱敏客户规模"]).unwrap_or_default(),
            150,
        ),
        objective: excerpt(
            field(&lines, &["投放目标", "活动目标", "传播目标", "生意目标"]).unwrap_or_default(),
            150,
        ),
        campaign_goal: field(&lines, &["Campaign 主目标", "Campaign目标", "主目标"])
            .and_then(campaign_goal)
            .unwrap_or_default()
            .to_owned(),
        audience: excerpt(
            field(&lines, &["目标人群", "受众画像"]).unwrap_or_default(),
            150,
        ),
        region: excerpt(
            field(&lines, &["投放区域", "投放城市", "目标区域", "目标商圈"]).unwrap_or_default(),
            150,
        ),
        monitoring: excerpt(
            field(&lines, &["监测要求", "上刊监测"]).unwrap_or(&monitor),
            150,
        ),
    };
    let required_routes = match field(
        &lines,
        &["指定线路", "客户指定线路", "必选线路", "投放线路"],
    )
    .map(super::plan::parse_required_routes)
    {
        Some(Ok(routes)) => routes,
        Some(Err(message)) => {
            issues.push(message);
            Vec::new()
        }
        None => Vec::new(),
    };
    let bus = bus || !required_routes.is_empty();
    if !bus && !metro && !outside_media {
        issues.push("没有匹配到赛题媒体，需选择公交车身或地铁出入口。");
    }
    let (starts, ends) = month
        .as_deref()
        .and_then(super::proposal_dates)
        .map_or((None, None), |(starts, ends)| (Some(starts), Some(ends)));
    Extraction {
        subject,
        industry,
        budget,
        budget_cap_wan,
        month,
        starts,
        ends,
        bus,
        metro,
        required_routes,
        outside_theme,
        brief,
        issues,
    }
}

fn docx_text(bytes: &[u8]) -> Option<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let file = archive.by_name("word/document.xml").ok()?;
    if file.size() > (BRIEF_MAX * 4) as u64 {
        return None;
    }
    let mut xml = String::new();
    file.take((BRIEF_MAX * 4) as u64)
        .read_to_string(&mut xml)
        .ok()?;
    let mut reader = Reader::from_str(&xml);
    let mut result = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().ok()? {
            Event::Start(element) if element.name().as_ref() == "w:t" => in_text = true,
            Event::End(element) if element.name().as_ref() == "w:t" => in_text = false,
            Event::End(element) if element.name().as_ref() == "w:p" => result.push('\n'),
            Event::Text(text) if in_text => result.push_str(&text.xml10_content()),
            Event::GeneralRef(reference) if in_text => {
                if let Some(character) = reference.resolve_char_ref().ok()? {
                    result.push(character);
                } else if let Some(entity) =
                    quick_xml::escape::resolve_predefined_entity(&reference.xml10_content())
                {
                    result.push_str(entity);
                }
            }
            Event::DocType(_) => return None,
            Event::Eof => break,
            _ => {}
        }
        if result.len() > BRIEF_MAX {
            return None;
        }
    }
    Some(result)
}

async fn read(mut multipart: Multipart) -> Result<Json<Extraction>, WebError> {
    let mut text = None;
    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(|_| INVALID)? {
        match field.name() {
            Some("text") if text.is_none() => {
                let value = field.text().await.map_err(|_| INVALID)?;
                if value.len() > BRIEF_MAX {
                    return Err(INVALID);
                }
                text = Some(value);
            }
            Some("file") if upload.is_none() => {
                let name = field.file_name().unwrap_or_default().to_ascii_lowercase();
                let bytes = field.bytes().await.map_err(|_| INVALID)?;
                if bytes.len() > BRIEF_MAX {
                    return Err(INVALID);
                }
                if !bytes.is_empty() {
                    upload = Some((name, bytes));
                }
            }
            _ => return Err(INVALID),
        }
    }
    let text = if let Some((name, bytes)) = upload {
        if text
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "请选择粘贴正文或上传附件其中一种。",
            ));
        }
        if name.ends_with(".docx") {
            tokio::task::spawn_blocking(move || docx_text(&bytes))
                .await
                .map_err(|_| INVALID)?
                .ok_or(INVALID)?
        } else if name.ends_with(".txt") {
            String::from_utf8(bytes.to_vec()).map_err(|_| INVALID)?
        } else {
            return Err(INVALID);
        }
    } else {
        text.ok_or(INVALID)?
    };
    if text.trim().is_empty() {
        return Err(INVALID);
    }
    Ok(Json(extract(text.trim_start_matches('\u{feff}'))))
}

pub(super) fn router() -> Router<Store> {
    Router::new().route(
        "/api/media-proposals/brief",
        post(read).layer(DefaultBodyLimit::max(BRIEF_MAX + 8192)),
    )
}
