use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::schedule::Window;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Operations {
    pub(super) source: String,
    pub(super) hours: String,
    pub(super) buses_per_night: u32,
    pub(super) exits_per_night: u32,
    pub(super) parallel: bool,
    pub(super) buffer: u32,
    windows: BTreeMap<String, Approved>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Approved {
    install: Nights,
    remove: Nights,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Nights {
    pub(super) from: String,
    pub(super) through: String,
}

pub(super) struct Draft<'a> {
    pub(super) source: &'a str,
    pub(super) hours: &'a str,
    pub(super) buses_per_night: &'a str,
    pub(super) exits_per_night: &'a str,
    pub(super) parallel: bool,
    pub(super) buffer: &'a str,
    pub(super) windows: &'a str,
    pub(super) attested: bool,
}

pub(super) struct Work {
    pub(super) dates: Nights,
    pub(super) available: u32,
    pub(super) shortage: u32,
    pub(super) outside_period: bool,
    pub(super) overlaps: bool,
}

impl Work {
    fn ready(&self) -> bool {
        self.shortage == 0 && !self.outside_period && !self.overlaps
    }
}

pub(super) struct Phase {
    pub(super) bus_nights: u32,
    pub(super) metro_nights: u32,
    pub(super) required: u32,
    pub(super) install: Work,
    pub(super) remove: Work,
}

impl Phase {
    pub(super) fn ready(&self) -> bool {
        self.required > 0 && self.install.ready() && self.remove.ready()
    }

    pub(super) fn steps(&self) -> [(&'static str, &Work); 2] {
        [("上刊", &self.install), ("下刊", &self.remove)]
    }
}

impl Operations {
    pub(super) fn parse(
        input: Draft<'_>,
        periods: &[Window],
    ) -> Result<Option<Self>, &'static str> {
        if input.source.trim().is_empty()
            && input.hours.trim().is_empty()
            && input.buses_per_night.trim().is_empty()
            && input.exits_per_night.trim().is_empty()
            && input.buffer.trim().is_empty()
            && input.windows.trim().is_empty()
            && !input.parallel
            && !input.attested
        {
            return Ok(None);
        }
        if !input.attested {
            return Err("请确认夜间窗口、产能和排班方式已与运营人员核对。");
        }
        let source = super::anonymize_food_case(input.source.trim()).into_owned();
        if source.is_empty() || source.chars().count() > 100 || source.chars().any(char::is_control)
        {
            return Err("请填写施工条件的核对依据，最多 100 个字符。");
        }
        let hours = input.hours.trim();
        night_hours(hours).ok_or("请按 HH:MM-HH:MM 填写经确认的夜间作业时段。")?;
        let capacity = |value: &str| {
            value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|value| (1..=10_000).contains(value))
        };
        let (Some(buses_per_night), Some(exits_per_night), Some(buffer)) = (
            capacity(input.buses_per_night),
            capacity(input.exits_per_night),
            input
                .buffer
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|value| *value <= 366),
        ) else {
            return Err("请填写两种媒体的单晚产能和每个窗口的缓冲晚数，可明确填 0 晚缓冲。");
        };
        if input.windows.chars().count() > 1200 {
            return Err("施工窗口说明总长度不得超过 1200 个字符。");
        }
        let mut windows = BTreeMap::new();
        for line in input
            .windows
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            const INVALID: &str = "请按「采购段开始日期 | 上刊起日至止日 | 下刊起日至止日」填写施工窗口，日期用 YYYY-MM-DD。";
            let mut fields = line.splitn(3, '|').map(str::trim);
            let start = fields.next().ok_or(INVALID)?;
            let install = fields.next().and_then(Nights::parse).ok_or(INVALID)?;
            let remove = fields.next().and_then(Nights::parse).ok_or(INVALID)?;
            if !periods
                .iter()
                .any(|period| period.needs_stock() && period.starts == start)
                || windows
                    .insert(start.into(), Approved { install, remove })
                    .is_some()
            {
                return Err("施工窗口须对应实际采购段的起始日期，同一采购段只填写一次。");
            }
        }
        if windows.is_empty() {
            return Err("请至少填写一个采购段的上刊和下刊获准窗口。");
        }
        Ok(Some(Self {
            source,
            hours: hours.into(),
            buses_per_night,
            exits_per_night,
            parallel: input.parallel,
            buffer,
            windows,
        }))
    }

    pub(super) fn phase(&self, starts: &str, ends: &str, buses: u32, exits: u32) -> Option<Phase> {
        let placement = self.windows.get(starts)?;
        let crosses_midnight = i32::from(night_hours(&self.hours)?);
        let first = day(starts)?;
        let last = day(ends)?;
        let bus_nights = buses.div_ceil(self.buses_per_night.max(1));
        let metro_nights = exits.div_ceil(self.exits_per_night.max(1));
        let required = if self.parallel {
            bus_nights.max(metro_nights)
        } else {
            bus_nights.saturating_add(metro_nights)
        };
        let needed = required.saturating_add(self.buffer);
        let measure = |dates: &Nights, install: bool| {
            let (from, through) = dates.days()?;
            // Date labels are the evening when work starts. Check the actual finishing date for overnight work.
            let outside_period = if install {
                through + crosses_midnight >= first
            } else {
                from <= last
            };
            let available = (through - from + 1) as u32;
            let overlaps = self
                .windows
                .iter()
                .filter(|(key, _)| key.as_str() != starts)
                .flat_map(|(_, approved)| [&approved.install, &approved.remove])
                .filter_map(Nights::days)
                .any(|(a, b)| from <= b && a <= through);
            Some(Work {
                dates: dates.clone(),
                available,
                shortage: needed.saturating_sub(available),
                outside_period,
                overlaps,
            })
        };
        Some(Phase {
            bus_nights,
            metro_nights,
            required,
            install: measure(&placement.install, true)?,
            remove: measure(&placement.remove, false)?,
        })
    }
}

impl Nights {
    fn parse(value: &str) -> Option<Self> {
        let (from, through) = value.split_once('至')?;
        let dates = Self {
            from: from.trim().into(),
            through: through.trim().into(),
        };
        dates.days().map(|_| dates)
    }

    fn days(&self) -> Option<(i32, i32)> {
        let (from, through) = (day(&self.from)?, day(&self.through)?);
        (from <= through).then_some((from, through))
    }
}

fn night_hours(hours: &str) -> Option<bool> {
    let (open, close) = hours.split_once('-')?;
    let minutes = |value: &str| {
        let (h, m) = value.split_once(':')?;
        if h.len() != 2
            || m.len() != 2
            || !h.bytes().chain(m.bytes()).all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let (open, close) = (minutes(open)?, minutes(close)?);
    (open != close).then_some(close < open)
}

fn day(date: &str) -> Option<i32> {
    let mut fields = date.split('-');
    let (year, month, day) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some()
        || year.len() != 4
        || month.len() != 2
        || day.len() != 2
        || !year
            .bytes()
            .chain(month.bytes())
            .chain(day.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let offset = match year {
        "2026" => -365,
        "2027" => 0,
        "2028" => 365,
        _ => return None,
    };
    let months = [
        31,
        28 + i32::from(year == "2028"),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let month = month.parse::<usize>().ok()?.checked_sub(1)?;
    let day = day.parse::<i32>().ok()?;
    (1..=*months.get(month)?)
        .contains(&day)
        .then(|| offset + months[..month].iter().sum::<i32>() + day - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::schedule::Rolling;

    #[test]
    fn planned_nights_account_for_shared_crews_rain_and_overnight_boundaries() {
        let periods = Rolling::showcase("2027-05-01 | 1 | 预热", "2027-01-01", "2027-12-31")
            .unwrap()
            .windows("2027-01-01", "2027-12-31")
            .unwrap();
        let mut rules = Operations::parse(
            Draft {
                source: "测试排班",
                hours: "22:00-05:00",
                buses_per_night: "4",
                exits_per_night: "1",
                buffer: "1",
                parallel: false,
                windows: "2027-05-01 | 2027-04-25 至 2027-04-28 | 2027-06-01 至 2027-06-04",
                attested: true,
            },
            &periods,
        )
        .unwrap()
        .unwrap();
        let shared = rules.phase("2027-05-01", "2027-05-31", 9, 1).unwrap();
        assert_eq!(
            (
                shared.bus_nights,
                shared.metro_nights,
                shared.required,
                shared.install.shortage
            ),
            (3, 1, 4, 1)
        );
        assert!(!shared.ready());
        rules.parallel = true;
        assert!(
            rules
                .phase("2027-05-01", "2027-05-31", 9, 1)
                .unwrap()
                .ready()
        );
        rules.windows.get_mut("2027-05-01").unwrap().install.through = "2027-04-30".into();
        let conflict = rules.phase("2027-05-01", "2027-05-31", 9, 1).unwrap();
        assert!(conflict.install.outside_period);
        assert!(!conflict.ready());
        assert_eq!(day("2028-01-01"), day("2027-12-31").map(|last| last + 1));
    }
}
