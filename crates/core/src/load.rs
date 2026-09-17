//! Open-loop load profiles: a rate curve (calls per second) that follows a
//! sequence of stages over wall-clock time.
//!
//! Where the `--benchmark` escalation is *closed-loop* (fire as fast as the
//! machine allows and see where the ceiling is), a load profile is
//! *open-loop*: requests are scheduled at a target rate that follows a curve,
//! independently of how fast responses come back.
//!
//! A [`LoadProfile`] is the declared form — a list of [`Stage`]s parsed from a
//! spec string (`"ramp:3m:30,hold:2m"`), a [preset](LoadProfile::preset) name,
//! or the equivalent JSON — kept serializable so it can be echoed back in a
//! report. Its rate math lives on the [compiled](LoadProfile::compile)
//! [`LoadCurve`]: the instantaneous rate [`cps_at`](LoadCurve::cps_at) and the
//! cumulative expected arrivals [`cumulative_arrivals`](LoadCurve::cumulative_arrivals)
//! `N(t) = \u{222b} cps(t) dt`, both closed-form per stage, that the scheduler
//! ticks against.
//!
//! Like the rest of this crate, the module is free of any I/O or timing
//! dependency: it turns a declaration into pure curve math that the `runner`
//! drives on a clock.

use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::fmt;
use std::time::Duration;

/// The rate curve a [`Stage`] traces from the previous stage's rate to its
/// target over the stage's duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// Linear from the previous stage's CPS (0 initially) to `target` — down
    /// as well as up, so a descent to 0 is expressible.
    Ramp,
    /// Constant: at `target` when given, otherwise at the previous stage's CPS.
    Hold,
    /// Jump instantly to `target` and hold it for the duration.
    Step,
    /// Oscillate between the previous stage's CPS and `target`, one full wave
    /// per `period`.
    Sine,
}

impl Shape {
    /// The lower-case spec keyword for this shape (`"ramp"`, `"hold"`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ramp => "ramp",
            Self::Hold => "hold",
            Self::Step => "step",
            Self::Sine => "sine",
        }
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stage of a load profile: a [`Shape`] held for `duration`.
///
/// `target_cps` is required for every shape except [`Shape::Hold`] (where it
/// defaults to the previous stage's rate); `period` applies only to
/// [`Shape::Sine`]. See [`LoadProfile::validate`] for the exact rules.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Stage {
    /// The rate curve traced over the stage.
    pub shape: Shape,
    /// Wall-clock length of the stage, parsed from a `"3m"` / `"30s"` string.
    #[serde(with = "duration_str")]
    pub duration: Duration,
    /// Target calls per second; meaning depends on the [`Shape`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cps: Option<f64>,
    /// Wave period for [`Shape::Sine`]; ignored (and rejected) otherwise.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_duration_str"
    )]
    pub period: Option<Duration>,
}

/// A declared load profile: the stages to run, in order.
///
/// Build one from a spec string or preset name with [`parse`](Self::parse),
/// or deserialize the JSON form (`{ "stages": [ ... ] }`) directly — in which
/// case call [`validate`](Self::validate) before use, since serde does not
/// enforce the per-shape field rules. Turn it into runnable curve math with
/// [`compile`](Self::compile).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LoadProfile {
    /// The stages, applied back to back from a starting rate of 0.
    pub stages: Vec<Stage>,
}

/// An error produced while parsing or validating a [`LoadProfile`].
#[derive(Debug, Clone, PartialEq)]
pub enum LoadProfileError {
    /// The spec or profile contained no stages.
    Empty,
    /// A bare name was given that is not a known preset.
    UnknownPreset {
        /// The name that was requested.
        name: String,
        /// The preset names that are available, sorted for a stable message.
        available: Vec<String>,
    },
    /// A stage's shape keyword is not one of `ramp`/`hold`/`step`/`sine`.
    UnknownShape(String),
    /// A stage had the wrong number of colon-separated fields.
    MalformedStage(String),
    /// A duration token could not be parsed (bad number or unit).
    InvalidDuration(String),
    /// A CPS token was not a finite, non-negative number.
    InvalidCps(String),
    /// A stage's duration was zero.
    ZeroDuration,
    /// A shape that requires an explicit target was given none.
    MissingTarget(Shape),
    /// A [`Shape::Sine`] stage was given no (positive) period.
    MissingPeriod,
    /// A period was given on a non-[`Shape::Sine`] stage.
    UnexpectedPeriod(Shape),
}

impl fmt::Display for LoadProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("load profile has no stages"),
            Self::UnknownPreset { name, available } => write!(
                f,
                "unknown load preset '{name}' (available: {})",
                available.join(", ")
            ),
            Self::UnknownShape(s) => {
                write!(
                    f,
                    "unknown stage shape '{s}' (expected ramp/hold/step/sine)"
                )
            }
            Self::MalformedStage(s) => write!(
                f,
                "malformed stage '{s}' (expected shape:duration[:target_cps[:period]])"
            ),
            Self::InvalidDuration(s) => {
                write!(f, "invalid duration '{s}' (expected e.g. 30s, 3m, 1h)")
            }
            Self::InvalidCps(s) => {
                write!(f, "invalid target_cps '{s}' (expected a number >= 0)")
            }
            Self::ZeroDuration => f.write_str("stage duration must be greater than zero"),
            Self::MissingTarget(shape) => write!(f, "'{shape}' stage requires a target_cps"),
            Self::MissingPeriod => f.write_str("'sine' stage requires a positive period"),
            Self::UnexpectedPeriod(shape) => {
                write!(f, "period is only valid on a 'sine' stage, not '{shape}'")
            }
        }
    }
}

impl std::error::Error for LoadProfileError {}

/// The five built-in presets, as (name, spec) pairs. They are defined via the
/// same grammar so they double as documentation.
const PRESETS: &[(&str, &str)] = &[
    ("smoke", "hold:30s:1"),
    ("ramp", "ramp:3m:30,hold:2m"),
    ("spike", "hold:1m:5,step:30s:50,hold:1m:5"),
    ("stairs", "step:1m:5,step:1m:10,step:1m:20,step:1m:40"),
    ("soak", "ramp:1m:10,hold:10m"),
];

impl LoadProfile {
    /// Parses a spec string or a preset name into a validated profile.
    ///
    /// A token with no `:` is looked up as a [preset](Self::preset) name
    /// (so `"ramp"` is the multi-stage ramp preset); anything containing a `:`
    /// is parsed as a custom spec of comma-separated `shape:duration[:target_cps[:period]]`
    /// stages (so `"ramp:3m:30"` is a single ramp stage).
    ///
    /// # Errors
    ///
    /// Returns a [`LoadProfileError`] describing the first malformed token or
    /// invalid stage.
    pub fn parse(spec: &str) -> Result<Self, LoadProfileError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(LoadProfileError::Empty);
        }
        if !spec.contains(':') {
            return Self::preset(spec).ok_or_else(|| LoadProfileError::UnknownPreset {
                name: spec.to_string(),
                available: PRESETS
                    .iter()
                    .map(|(name, _)| (*name).to_string())
                    .collect(),
            });
        }
        let profile = Self::parse_stages(spec)?;
        profile.validate()?;
        Ok(profile)
    }

    /// Returns the built-in preset with this name, already validated, or
    /// `None` if the name is unknown.
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        let (_, spec) = PRESETS.iter().find(|(preset, _)| *preset == name)?;
        let profile = Self::parse_stages(spec).expect("built-in preset spec is valid");
        Some(profile)
    }

    /// Parses a custom spec of comma-separated stages, without preset lookup.
    fn parse_stages(spec: &str) -> Result<Self, LoadProfileError> {
        let stages = spec
            .split(',')
            .map(|raw| parse_stage(raw.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { stages })
    }

    /// Checks the per-shape field rules serde cannot express.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadProfileError`] for the first offending stage: an empty
    /// profile, a zero duration, a missing required target or period, an
    /// unexpected period, or a non-finite / negative target.
    pub fn validate(&self) -> Result<(), LoadProfileError> {
        if self.stages.is_empty() {
            return Err(LoadProfileError::Empty);
        }
        for stage in &self.stages {
            if stage.duration.is_zero() {
                return Err(LoadProfileError::ZeroDuration);
            }
            match stage.shape {
                Shape::Ramp | Shape::Step if stage.target_cps.is_none() => {
                    return Err(LoadProfileError::MissingTarget(stage.shape));
                }
                Shape::Sine => {
                    if stage.target_cps.is_none() {
                        return Err(LoadProfileError::MissingTarget(stage.shape));
                    }
                    if stage.period.is_none_or(|p| p.is_zero()) {
                        return Err(LoadProfileError::MissingPeriod);
                    }
                }
                _ => {}
            }
            if stage.shape != Shape::Sine && stage.period.is_some() {
                return Err(LoadProfileError::UnexpectedPeriod(stage.shape));
            }
            if let Some(cps) = stage.target_cps
                && (!cps.is_finite() || cps < 0.0)
            {
                return Err(LoadProfileError::InvalidCps(cps.to_string()));
            }
        }
        Ok(())
    }

    /// Compiles the declared stages into an absolute-time [`LoadCurve`],
    /// resolving each stage's start rate from the previous stage's end rate
    /// (starting from 0).
    #[must_use]
    pub fn compile(&self) -> LoadCurve {
        let mut segments = Vec::with_capacity(self.stages.len());
        let mut prev_cps = 0.0;
        let mut start_s = 0.0;
        for stage in &self.stages {
            let duration_s = stage.duration.as_secs_f64();
            let curve = match stage.shape {
                Shape::Ramp => Curve::Linear {
                    start_cps: prev_cps,
                    end_cps: stage.target_cps.unwrap_or(prev_cps),
                },
                Shape::Hold | Shape::Step => {
                    let level = stage.target_cps.unwrap_or(prev_cps);
                    Curve::Linear {
                        start_cps: level,
                        end_cps: level,
                    }
                }
                Shape::Sine => Curve::Sine {
                    start_cps: prev_cps,
                    target_cps: stage.target_cps.unwrap_or(prev_cps),
                    period_s: stage.period.map_or(duration_s, |p| p.as_secs_f64()),
                },
            };
            let segment = Segment {
                start_s,
                duration_s,
                curve,
            };
            prev_cps = segment.end_cps();
            start_s += duration_s;
            segments.push(segment);
        }
        LoadCurve {
            segments,
            total_s: start_s,
        }
    }
}

/// A compiled load profile: absolute-time segments ready for rate math.
///
/// Built by [`LoadProfile::compile`]. Compile once and reuse — the rate
/// queries are read-only, so a scheduler can tick against a single curve.
#[derive(Debug, Clone)]
pub struct LoadCurve {
    segments: Vec<Segment>,
    total_s: f64,
}

impl LoadCurve {
    /// Total wall-clock length of the profile.
    #[must_use]
    pub fn total_duration(&self) -> Duration {
        Duration::from_secs_f64(self.total_s)
    }

    /// Instantaneous target rate (calls per second) at offset `t` from the
    /// profile start. Zero at or past the end.
    #[must_use]
    pub fn cps_at(&self, t: Duration) -> f64 {
        let secs = t.as_secs_f64();
        for segment in &self.segments {
            if secs < segment.end_s() {
                return segment.cps_at_local((secs - segment.start_s).max(0.0));
            }
        }
        0.0
    }

    /// Cumulative expected arrivals `N(t) = \u{222b} cps` from the profile
    /// start to offset `t` — the request budget the scheduler fires against.
    /// Clamps to the full profile past the end.
    #[must_use]
    pub fn cumulative_arrivals(&self, t: Duration) -> f64 {
        let secs = t.as_secs_f64();
        let mut total = 0.0;
        for segment in &self.segments {
            if secs >= segment.end_s() {
                total += segment.arrivals_until(segment.duration_s);
            } else {
                total += segment.arrivals_until(secs - segment.start_s);
                break;
            }
        }
        total
    }
}

/// One compiled stage placed on the absolute timeline.
#[derive(Debug, Clone)]
struct Segment {
    start_s: f64,
    duration_s: f64,
    curve: Curve,
}

/// The closed-form rate curve of a [`Segment`], local to its own start.
#[derive(Debug, Clone)]
enum Curve {
    /// Linear interpolation from `start_cps` to `end_cps` (covers ramp, and
    /// hold/step where the two are equal).
    Linear { start_cps: f64, end_cps: f64 },
    /// Cosine wave from `start_cps` up to `target_cps` and back, one full wave
    /// per `period_s`.
    Sine {
        start_cps: f64,
        target_cps: f64,
        period_s: f64,
    },
}

impl Segment {
    /// Absolute end offset of the segment.
    fn end_s(&self) -> f64 {
        self.start_s + self.duration_s
    }

    /// Rate at `local` seconds into the segment.
    fn cps_at_local(&self, local: f64) -> f64 {
        match self.curve {
            Curve::Linear { start_cps, end_cps } => {
                if self.duration_s <= 0.0 {
                    start_cps
                } else {
                    start_cps + (end_cps - start_cps) * (local / self.duration_s)
                }
            }
            Curve::Sine {
                start_cps,
                target_cps,
                period_s,
            } => {
                if period_s <= 0.0 {
                    return (start_cps + target_cps) / 2.0;
                }
                let mid = (start_cps + target_cps) / 2.0;
                let amplitude = (target_cps - start_cps) / 2.0;
                mid - amplitude * (TAU * local / period_s).cos()
            }
        }
    }

    /// Integral of the rate over `[0, local]` (clamped to the segment).
    fn arrivals_until(&self, local: f64) -> f64 {
        let local = local.clamp(0.0, self.duration_s);
        match self.curve {
            Curve::Linear { start_cps, end_cps } => {
                if self.duration_s <= 0.0 {
                    return 0.0;
                }
                let slope = (end_cps - start_cps) / self.duration_s;
                start_cps * local + slope * local * local / 2.0
            }
            Curve::Sine {
                start_cps,
                target_cps,
                period_s,
            } => {
                if period_s <= 0.0 {
                    return (start_cps + target_cps) / 2.0 * local;
                }
                let mid = (start_cps + target_cps) / 2.0;
                let amplitude = (target_cps - start_cps) / 2.0;
                mid * local - amplitude * (period_s / TAU) * (TAU * local / period_s).sin()
            }
        }
    }

    /// Rate at the very end of the segment; the next stage starts from here.
    fn end_cps(&self) -> f64 {
        self.cps_at_local(self.duration_s)
    }
}

/// Parses one `shape:duration[:target_cps[:period]]` stage.
fn parse_stage(raw: &str) -> Result<Stage, LoadProfileError> {
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return Err(LoadProfileError::MalformedStage(raw.to_string()));
    }
    let shape = parse_shape(parts[0])?;
    let duration = parse_duration(parts[1])?;
    let target_cps = match parts.get(2) {
        Some(token) if !token.is_empty() => Some(parse_cps(token)?),
        _ => None,
    };
    let period = match parts.get(3) {
        Some(token) if !token.is_empty() => Some(parse_duration(token)?),
        _ => None,
    };
    Ok(Stage {
        shape,
        duration,
        target_cps,
        period,
    })
}

/// Parses a shape keyword.
fn parse_shape(token: &str) -> Result<Shape, LoadProfileError> {
    match token {
        "ramp" => Ok(Shape::Ramp),
        "hold" => Ok(Shape::Hold),
        "step" => Ok(Shape::Step),
        "sine" => Ok(Shape::Sine),
        other => Err(LoadProfileError::UnknownShape(other.to_string())),
    }
}

/// Parses a `<number><unit>` duration, where unit is `s`, `m`, or `h`.
fn parse_duration(token: &str) -> Result<Duration, LoadProfileError> {
    let token = token.trim();
    let invalid = || LoadProfileError::InvalidDuration(token.to_string());
    let (number, unit) = token.split_at(token.len().checked_sub(1).ok_or_else(invalid)?);
    let multiplier = match unit {
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return Err(invalid()),
    };
    let value: f64 = number.parse().map_err(|_| invalid())?;
    if !value.is_finite() || value < 0.0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs_f64(value * multiplier))
}

/// Parses a target-CPS token: a finite, non-negative number.
fn parse_cps(token: &str) -> Result<f64, LoadProfileError> {
    let value: f64 = token
        .parse()
        .map_err(|_| LoadProfileError::InvalidCps(token.to_string()))?;
    if !value.is_finite() || value < 0.0 {
        return Err(LoadProfileError::InvalidCps(token.to_string()));
    }
    Ok(value)
}

/// Renders a whole-second duration as the largest exact `h`/`m`/`s` unit,
/// falling back to fractional seconds. Inverse of [`parse_duration`] for the
/// values it emits.
fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs_f64();
    if total.fract() == 0.0 {
        let secs = total as u64;
        if secs != 0 && secs.is_multiple_of(3600) {
            return format!("{}h", secs / 3600);
        }
        if secs != 0 && secs.is_multiple_of(60) {
            return format!("{}m", secs / 60);
        }
        return format!("{secs}s");
    }
    format!("{total}s")
}

/// serde adapter: a [`Duration`] as a `"3m"`-style string.
mod duration_str {
    use super::{Duration, format_duration, parse_duration};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(*d))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

/// serde adapter: an optional [`Duration`] as a `"1m"`-style string.
mod opt_duration_str {
    use super::{Duration, format_duration, parse_duration};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(duration) => s.serialize_some(&format_duration(*duration)),
            None => s.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Duration>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|text| parse_duration(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute tolerance for the floating-point rate math.
    const EPS: f64 = 1e-9;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    // ---------- spec-string parsing ----------

    #[test]
    fn parses_a_single_ramp_stage() {
        let profile = LoadProfile::parse("ramp:3m:30").unwrap();
        assert_eq!(profile.stages.len(), 1);
        let stage = &profile.stages[0];
        assert_eq!(stage.shape, Shape::Ramp);
        assert_eq!(stage.duration, secs(180.0));
        assert_eq!(stage.target_cps, Some(30.0));
        assert_eq!(stage.period, None);
    }

    #[test]
    fn parses_a_hold_without_target() {
        let profile = LoadProfile::parse("hold:45s").unwrap();
        let stage = &profile.stages[0];
        assert_eq!(stage.shape, Shape::Hold);
        assert_eq!(stage.duration, secs(45.0));
        assert_eq!(stage.target_cps, None);
    }

    #[test]
    fn parses_a_sine_with_period() {
        let profile = LoadProfile::parse("sine:5m:30:1m").unwrap();
        let stage = &profile.stages[0];
        assert_eq!(stage.shape, Shape::Sine);
        assert_eq!(stage.duration, secs(300.0));
        assert_eq!(stage.target_cps, Some(30.0));
        assert_eq!(stage.period, Some(secs(60.0)));
    }

    #[test]
    fn parses_multiple_comma_separated_stages() {
        let profile = LoadProfile::parse("ramp:3m:30, hold:2m").unwrap();
        assert_eq!(profile.stages.len(), 2);
        assert_eq!(profile.stages[1].shape, Shape::Hold);
        assert_eq!(profile.stages[1].duration, secs(120.0));
    }

    #[test]
    fn duration_units_seconds_minutes_hours() {
        assert_eq!(
            LoadProfile::parse("hold:90s").unwrap().stages[0].duration,
            secs(90.0)
        );
        assert_eq!(
            LoadProfile::parse("hold:2m").unwrap().stages[0].duration,
            secs(120.0)
        );
        assert_eq!(
            LoadProfile::parse("hold:1h").unwrap().stages[0].duration,
            secs(3600.0)
        );
        // fractional mantissa is accepted
        assert_eq!(
            LoadProfile::parse("hold:1.5m").unwrap().stages[0].duration,
            secs(90.0)
        );
    }

    // ---------- presets ----------

    #[test]
    fn every_preset_parses_and_validates() {
        for (name, _) in PRESETS {
            let profile = LoadProfile::preset(name).unwrap();
            profile.validate().unwrap();
            assert_eq!(LoadProfile::parse(name).unwrap(), profile, "{name}");
        }
    }

    #[test]
    fn smoke_preset_holds_one_cps() {
        let profile = LoadProfile::preset("smoke").unwrap();
        assert_eq!(profile.stages.len(), 1);
        assert_eq!(profile.stages[0].shape, Shape::Hold);
        assert_eq!(profile.stages[0].target_cps, Some(1.0));
    }

    #[test]
    fn bare_ramp_is_the_preset_not_a_single_stage() {
        // A colon-free "ramp" is the two-stage preset; "ramp:3m:30" is one stage.
        assert_eq!(LoadProfile::parse("ramp").unwrap().stages.len(), 2);
        assert_eq!(LoadProfile::parse("ramp:3m:30").unwrap().stages.len(), 1);
    }

    #[test]
    fn unknown_preset_lists_available() {
        let err = LoadProfile::parse("nope").unwrap_err();
        match err {
            LoadProfileError::UnknownPreset { name, available } => {
                assert_eq!(name, "nope");
                assert!(available.contains(&"smoke".to_string()));
            }
            other => panic!("expected UnknownPreset, got {other:?}"),
        }
    }

    // ---------- error cases ----------

    #[test]
    fn empty_spec_is_rejected() {
        assert_eq!(
            LoadProfile::parse("   ").unwrap_err(),
            LoadProfileError::Empty
        );
    }

    #[test]
    fn unknown_shape_is_rejected() {
        assert_eq!(
            LoadProfile::parse("wat:3m").unwrap_err(),
            LoadProfileError::UnknownShape("wat".to_string())
        );
    }

    #[test]
    fn too_many_fields_is_malformed() {
        assert!(matches!(
            LoadProfile::parse("ramp:3m:30:1m:extra").unwrap_err(),
            LoadProfileError::MalformedStage(_)
        ));
    }

    #[test]
    fn bad_duration_unit_is_rejected() {
        assert!(matches!(
            LoadProfile::parse("ramp:3x:30").unwrap_err(),
            LoadProfileError::InvalidDuration(_)
        ));
    }

    #[test]
    fn negative_cps_is_rejected() {
        assert!(matches!(
            LoadProfile::parse("ramp:3m:-5").unwrap_err(),
            LoadProfileError::InvalidCps(_)
        ));
    }

    #[test]
    fn ramp_and_step_require_a_target() {
        assert_eq!(
            LoadProfile::parse("ramp:3m").unwrap_err(),
            LoadProfileError::MissingTarget(Shape::Ramp)
        );
        assert_eq!(
            LoadProfile::parse("step:1m").unwrap_err(),
            LoadProfileError::MissingTarget(Shape::Step)
        );
    }

    #[test]
    fn sine_requires_a_period() {
        assert_eq!(
            LoadProfile::parse("sine:5m:30").unwrap_err(),
            LoadProfileError::MissingPeriod
        );
    }

    #[test]
    fn period_on_non_sine_is_rejected() {
        assert_eq!(
            LoadProfile::parse("hold:3m:5:1m").unwrap_err(),
            LoadProfileError::UnexpectedPeriod(Shape::Hold)
        );
    }

    #[test]
    fn zero_duration_is_rejected() {
        assert_eq!(
            LoadProfile::parse("ramp:0s:30").unwrap_err(),
            LoadProfileError::ZeroDuration
        );
    }

    // ---------- JSON parsing ----------

    #[test]
    fn json_form_matches_the_spec_string() {
        let json = serde_json::json!({
            "stages": [
                { "shape": "ramp", "duration": "3m", "target_cps": 30 },
                { "shape": "hold", "duration": "2m" }
            ]
        });
        let from_json: LoadProfile = serde_json::from_value(json).unwrap();
        from_json.validate().unwrap();
        assert_eq!(from_json, LoadProfile::parse("ramp:3m:30,hold:2m").unwrap());
    }

    #[test]
    fn json_round_trips_through_serialization() {
        let profile = LoadProfile::parse("ramp:3m:30,hold:2m,sine:1m:20:30s").unwrap();
        let text = serde_json::to_string(&profile).unwrap();
        let back: LoadProfile = serde_json::from_str(&text).unwrap();
        assert_eq!(profile, back);
    }

    // ---------- rate math: cps_at ----------

    #[test]
    fn ramp_then_hold_rate_over_time() {
        let curve = LoadProfile::parse("ramp:3m:30,hold:2m").unwrap().compile();
        assert_close(curve.cps_at(secs(0.0)), 0.0);
        assert_close(curve.cps_at(secs(90.0)), 15.0); // halfway up the ramp
        assert_close(curve.cps_at(secs(180.0)), 30.0); // hold picks up the ramp's end
        assert_close(curve.cps_at(secs(240.0)), 30.0);
        assert_close(curve.cps_at(secs(300.0)), 0.0); // at/after the end
        assert_eq!(curve.total_duration(), secs(300.0));
    }

    #[test]
    fn step_rate_is_flat_from_zero() {
        let curve = LoadProfile::parse("step:10s:5").unwrap().compile();
        assert_close(curve.cps_at(secs(0.0)), 5.0);
        assert_close(curve.cps_at(secs(3.0)), 5.0);
    }

    #[test]
    fn sine_oscillates_between_previous_and_target() {
        // From 0 to 20, one wave per 60s: 0 at t=0, 10 at t=15, 20 at t=30, 0 at t=60.
        let curve = LoadProfile::parse("sine:60s:20:60s").unwrap().compile();
        assert_close(curve.cps_at(secs(0.0)), 0.0);
        assert_close(curve.cps_at(secs(15.0)), 10.0);
        assert_close(curve.cps_at(secs(30.0)), 20.0);
        assert_close(curve.cps_at(secs(45.0)), 10.0);
    }

    // ---------- rate math: cumulative arrivals ----------

    #[test]
    fn ramp_arrivals_are_the_triangle_area() {
        let curve = LoadProfile::parse("ramp:3m:30,hold:2m").unwrap().compile();
        // Triangle 0->15 over 90s = 0.5 * 90 * 15 = 675.
        assert_close(curve.cumulative_arrivals(secs(90.0)), 675.0);
        // Full ramp 0->30 over 180s = 0.5 * 180 * 30 = 2700.
        assert_close(curve.cumulative_arrivals(secs(180.0)), 2700.0);
        // Plus a 120s hold at 30 = 3600 -> 6300 total, clamped past the end.
        assert_close(curve.cumulative_arrivals(secs(300.0)), 6300.0);
        assert_close(curve.cumulative_arrivals(secs(1000.0)), 6300.0);
    }

    #[test]
    fn step_arrivals_are_rate_times_time() {
        let curve = LoadProfile::parse("step:10s:5").unwrap().compile();
        assert_close(curve.cumulative_arrivals(secs(4.0)), 20.0);
        assert_close(curve.cumulative_arrivals(secs(10.0)), 50.0);
    }

    #[test]
    fn sine_arrivals_over_a_full_wave_are_the_mean_times_duration() {
        // Mean of the wave is the midpoint (10); over one 60s period: 600.
        let curve = LoadProfile::parse("sine:60s:20:60s").unwrap().compile();
        assert_close(curve.cumulative_arrivals(secs(60.0)), 600.0);
    }

    #[test]
    fn cumulative_arrivals_are_monotonic() {
        let curve = LoadProfile::parse("spike").unwrap().compile();
        let mut previous = 0.0;
        let mut t = 0.0;
        while t <= 160.0 {
            let n = curve.cumulative_arrivals(secs(t));
            assert!(n + EPS >= previous, "N dipped at t={t}: {n} < {previous}");
            previous = n;
            t += 0.5;
        }
    }
}
