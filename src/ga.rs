//! Thin client over the GA4 Data API (`runReport`, `runRealtimeReport`) and
//! the Admin API (`accountSummaries`). One endpoint family, so a hand-rolled
//! client beats pulling in a generated SDK.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth::Auth;

const DATA_API: &str = "https://analyticsdata.googleapis.com/v1beta";
const ADMIN_API: &str = "https://analyticsadmin.googleapis.com/v1beta";

// ---------------------------------------------------------------- request ---

#[derive(Serialize)]
pub struct DateRange {
    #[serde(rename = "startDate")]
    pub start_date: String,
    #[serde(rename = "endDate")]
    pub end_date: String,
}

impl DateRange {
    /// GA4 accepts relative dates; `NdaysAgo` keeps us off local-clock math.
    /// `yesterday` is the end because today's data is still partial.
    pub fn last_days(days: u32) -> DateRange {
        DateRange {
            start_date: format!("{}daysAgo", days),
            end_date: "yesterday".to_string(),
        }
    }

    /// The equivalent window immediately before `last_days`, for deltas.
    pub fn previous_days(days: u32) -> DateRange {
        DateRange {
            start_date: format!("{}daysAgo", days * 2),
            end_date: format!("{}daysAgo", days + 1),
        }
    }
}

#[derive(Serialize)]
pub struct Named {
    pub name: String,
}

impl Named {
    pub fn list(names: &[&str]) -> Vec<Named> {
        names
            .iter()
            .map(|n| Named {
                name: n.to_string(),
            })
            .collect()
    }
}

#[derive(Serialize)]
pub struct MetricOrderBy {
    #[serde(rename = "metricName")]
    pub metric_name: String,
}

#[derive(Serialize)]
pub struct OrderBy {
    pub metric: MetricOrderBy,
    pub desc: bool,
}

impl OrderBy {
    pub fn desc(metric: &str) -> OrderBy {
        OrderBy {
            metric: MetricOrderBy {
                metric_name: metric.to_string(),
            },
            desc: true,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub date_ranges: Vec<DateRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<Named>,
    pub metrics: Vec<Named>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order_bys: Vec<OrderBy>,
}

impl ReportRequest {
    pub fn new(metrics: &[&str]) -> ReportRequest {
        ReportRequest {
            date_ranges: Vec::new(),
            dimensions: Vec::new(),
            metrics: Named::list(metrics),
            limit: None,
            order_bys: Vec::new(),
        }
    }

    pub fn range(mut self, range: DateRange) -> Self {
        self.date_ranges = vec![range];
        self
    }

    pub fn by(mut self, dimensions: &[&str]) -> Self {
        self.dimensions = Named::list(dimensions);
        self
    }

    pub fn top(mut self, metric: &str, limit: i64) -> Self {
        self.order_bys = vec![OrderBy::desc(metric)];
        self.limit = Some(limit);
        self
    }
}

// --------------------------------------------------------------- response ---

#[derive(Deserialize, Default)]
#[allow(dead_code)]
pub struct Header {
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct Cell {
    #[serde(default)]
    pub value: String,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    #[serde(default)]
    pub dimension_values: Vec<Cell>,
    #[serde(default)]
    pub metric_values: Vec<Cell>,
}

impl Row {
    pub fn dimension(&self, i: usize) -> &str {
        self.dimension_values
            .get(i)
            .map(|c| c.value.as_str())
            .unwrap_or("(none)")
    }

    pub fn metric(&self, i: usize) -> f64 {
        self.metric_values
            .get(i)
            .and_then(|c| c.value.parse::<f64>().ok())
            .unwrap_or(0.0)
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // headers/row_count kept for forthcoming table views
pub struct Report {
    #[serde(default)]
    pub dimension_headers: Vec<Header>,
    #[serde(default)]
    pub metric_headers: Vec<Header>,
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub totals: Vec<Row>,
    #[serde(default)]
    pub row_count: i64,
}

impl Report {
    /// Value of metric `i` summed across the whole report, as GA computed it.
    /// Falls back to summing rows when the API omits a totals row.
    pub fn total(&self, i: usize) -> f64 {
        if let Some(row) = self.totals.first() {
            return row.metric(i);
        }
        self.rows.iter().map(|r| r.metric(i)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

// ----------------------------------------------------------------- client ---

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Deserialize)]
#[allow(dead_code)] // `status` is useful when debugging raw API errors
struct ApiErrorBody {
    message: String,
    #[serde(default)]
    status: String,
}

pub struct Ga {
    http: reqwest::Client,
    auth: Auth,
}

impl Ga {
    pub fn new() -> Result<Ga> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("anacraft/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let auth = Auth::new(http.clone())?;
        Ok(Ga { http, auth })
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        let token = self.auth.access_token().await?;
        let res = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .context("calling the Google Analytics API")?;

        let status = res.status();
        let text = res.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("{}", explain(status.as_u16(), &text));
        }

        serde_json::from_str(&text).context("unexpected response shape from Google")
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let token = self.auth.access_token().await?;
        let res = self.http.get(url).bearer_auth(token).send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("{}", explain(status.as_u16(), &text));
        }
        serde_json::from_str(&text).context("unexpected response shape from Google")
    }

    /// Run a report, transparently retrying with the legacy `conversions`
    /// metric for properties that predate the `keyEvents` rename.
    pub async fn report(&self, property: &str, req: ReportRequest) -> Result<Report> {
        let url = format!("{DATA_API}/properties/{property}:runReport");
        match self.post::<Report>(&url, &req).await {
            Ok(report) => Ok(report),
            Err(err) => {
                let msg = err.to_string();
                let uses_key_events = req.metrics.iter().any(|m| m.name == "keyEvents");
                if uses_key_events && msg.contains("keyEvents") {
                    let mut retry = req;
                    for metric in retry.metrics.iter_mut() {
                        if metric.name == "keyEvents" {
                            metric.name = "conversions".to_string();
                        }
                    }
                    for order in retry.order_bys.iter_mut() {
                        if order.metric.metric_name == "keyEvents" {
                            order.metric.metric_name = "conversions".to_string();
                        }
                    }
                    return self.post::<Report>(&url, &retry).await;
                }
                Err(err)
            }
        }
    }

    pub async fn realtime(&self, property: &str, req: ReportRequest) -> Result<Report> {
        let url = format!("{DATA_API}/properties/{property}:runRealtimeReport");
        self.post::<Report>(&url, &req).await
    }

    /// Every property the signed-in account can read.
    pub async fn properties(&self) -> Result<Vec<Property>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!("{ADMIN_API}/accountSummaries?pageSize=200");
            if let Some(token) = &page_token {
                url.push_str(&format!("&pageToken={token}"));
            }
            let page: AccountSummaries = self.get(&url).await?;

            for account in page.account_summaries {
                for prop in account.property_summaries {
                    out.push(Property {
                        id: prop.property.trim_start_matches("properties/").to_string(),
                        name: prop.display_name,
                        account: account.display_name.clone(),
                    });
                }
            }

            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(out)
    }
}

pub struct Property {
    pub id: String,
    pub name: String,
    pub account: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountSummaries {
    #[serde(default)]
    account_summaries: Vec<AccountSummary>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountSummary {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    property_summaries: Vec<PropertySummary>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PropertySummary {
    #[serde(default)]
    property: String,
    #[serde(default)]
    display_name: String,
}

/// Turn Google's error payloads into something a user can act on.
fn explain(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<ApiError>(body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| body.chars().take(300).collect());

    match status {
        401 => format!("login expired — run `anacraft login`\n  ({detail})"),
        403 if detail.contains("has not been used") || detail.contains("is disabled") => format!(
            "an API isn't enabled on your Google Cloud project.\n  \
             Enable both the Google Analytics Data API and Admin API, then retry.\n  ({detail})"
        ),
        403 => format!(
            "access denied — the signed-in account needs at least Viewer on this property.\n  ({detail})"
        ),
        429 => format!("Google rate-limited this request; try again shortly.\n  ({detail})"),
        _ => format!("Google Analytics API error {status}: {detail}"),
    }
}
