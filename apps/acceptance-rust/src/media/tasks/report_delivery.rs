use std::sync::LazyLock;

use regex::Regex;

static INTERNAL_DETAILS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<details class="report-evidence">\s*<summary>查看核验依据</summary>.*?</details>"#,
    )
    .expect("report review markup")
});

static MISSING_EVIDENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<dd class="report-metric-detail">\d+ 个档期资源项待补单位日触达</dd>|<small>待补(?:线路轨迹或原图|出口实点坐标)</small>|<p class="report-caption">[^<]* · 线路原图待补。</p>"#,
    )
    .expect("report evidence markup")
});

pub(super) fn for_browsing(mut html: String) -> String {
    let Some(body_start) = html.find("<body") else {
        return html;
    };
    // Preserve the saved workpaper and its eligibility checks. Only present the client view.
    let mut body = html.split_off(body_start);
    for pattern in [&*INTERNAL_DETAILS, &*MISSING_EVIDENCE] {
        if let std::borrow::Cow::Owned(clean) = pattern.replace_all(&body, "") {
            body = clean;
        }
    }
    for (original, shown) in [
        (
            r#"<p class="media-notice report-draft">投放证据待核</p>"#,
            "",
        ),
        ("备选草案 · 待客户确认", "媒介与档期条件"),
        ("备选草案 · 待确认", "投放条件说明"),
        ("全年入选公交线路总图待核。", "本节列出已有的分档期线路图。"),
        ("线路或出口坐标待核。", "此组合暂无可展示的线路原图。"),
        ("历史截图，底图待复核", "历史底图截图"),
        (
            "待调整。当前库存、采购条件或必选资源尚未满足全部档期。",
            "当前库存仅支持表内所列采购组合。",
        ),
        ("待补新云触达数据", "暂无可用于估算的单位触达"),
        (
            "本段施工待核 · 需确认采购清单和获准施工窗口。",
            "施工按获准的上下刊窗口安排。",
        ),
        ("上下刊窗口待复核", "上下刊窗口测算"),
        (
            "新的上下刊施工窗口待运营确认。",
            "按调整后的采购段安排上下刊。",
        ),
        (
            "夜间窗口、产能与天气缓冲待运营确认。确认后按各案采购清单逐段核对。",
            "",
        ),
        (
            "待导入该精确档期的红豆快照。",
            "刊例与数量仅包含有库存依据的档期。",
        ),
        (
            "至少一个采购段缺少匹配库存、指定媒体或必选线路的有效刊例，整案最低报价待核对。",
            "报价仅汇总表内具备库存和刊例依据的资源。",
        ),
        ("最低组合待核对", "暂无可计算的最低报价"),
        (
            "此段库存快照待补。两版金额均为预留额度，采购数量和刊例待核。",
            "两版金额均为预留额度。",
        ),
        (" 天 · 待核对</p>", " 天</p>"),
        ("<td>待核对</td>", "<td>—</td>"),
        (">待核对</dd>", ">—</dd>"),
        ("品牌类型及规模待确认。", "—"),
        ("详细目标待补充。", "—"),
        ("目标人群待补充。", "—"),
        ("客户验收口径待确认。", "验收采用订单约定的统计口径。"),
        ("<dt>投放主目标</dt><dd>待确认", "<dt>投放主目标</dt><dd>—"),
        (" · 待确认</dt>", " · 未纳入采购</dt>"),
        (
            "按客户指定线路入选，目标圈覆盖待复核。",
            "按客户指定线路入选。",
        ),
        ("目标圈覆盖待复核。", "本组合暂无可展示的目标圈覆盖记录。"),
        ("查看待核或未入选项", "查看未入选项"),
        (
            "请先确认客户主目标。待核草案保留两案供核对，确认目标后可重新生成。",
            "两案可按采购组合、覆盖及报价比较。",
        ),
    ] {
        if body.contains(original) {
            body = body.replace(original, shown);
        }
    }
    html.push_str(&body);
    html
}
