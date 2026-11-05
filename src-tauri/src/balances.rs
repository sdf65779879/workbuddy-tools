//! Account balances from the billing resource API. Never infer remaining credits from unrelated fields.
use crate::config::AccountConfig;
use base64::{engine::general_purpose, Engine};
use serde_json::{json, Value};
use std::borrow::Cow;

#[derive(Debug, Clone, Default)]
pub struct BalanceObservation {
    pub remaining: Option<f64>,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub unlimited: bool,
}

pub fn billing_base(configured: &str, realm: &str) -> Result<String, String> {
    if realm != "cn" && realm != "global" {
        return Err("账号区域必须为 cn 或 global".into());
    }
    let parsed = reqwest::Url::parse(configured).map_err(|_| "计费服务地址无效".to_string())?;
    let official = matches!(
        parsed.host_str(),
        Some(
            "www.workbuddy.cn"
                | "workbuddy.cn"
                | "www.codebuddy.cn"
                | "codebuddy.cn"
                | "copilot.tencent.com"
                | "www.workbuddy.ai"
                | "workbuddy.ai"
        )
    );
    // Respect explicitly configured proxies; official hosts use their actual billing domain.
    Ok(if official {
        if realm == "global" {
            "https://www.workbuddy.ai".into()
        } else {
            "https://www.codebuddy.cn".into()
        }
    } else {
        configured.trim_end_matches('/').into()
    })
}

fn resource_paths(account: &AccountConfig) -> Vec<&'static str> {
    if !account.enterprise_id.trim().is_empty() {
        vec!["/v2/billing/meter/get-enterprise-user-usage"]
    } else if account.realm == "global" {
        vec![
            "/billing/meter/get-user-resource",
            "/v2/billing/meter/get-user-resource",
        ]
    } else {
        vec!["/v2/billing/meter/get-user-resource"]
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|n| n.is_finite())
}

fn unwrap_data(response: &Value) -> Result<&Value, String> {
    if let Some(code) = response.get("code") {
        let code = number(code).ok_or_else(|| "余额响应的业务状态码无效".to_string())?;
        if code != 0.0 && code != 200.0 {
            return Err(format!("余额查询失败（业务状态码 {code}）"));
        }
    }
    let data = response.get("data").unwrap_or(response);
    if let Some(error) = data.pointer("/Response/Error") {
        if !error.is_null() {
            return Err("余额资源接口返回业务错误".into());
        }
    }
    Ok(data)
}

fn package_balance(package: &Value) -> Result<BalanceObservation, String> {
    let field = |key: &str| package.get(key).and_then(number);
    let cycle_size = field("CycleCapacitySize");
    let prefix = if cycle_size.is_some_and(|size| size > 0.0) {
        "CycleCapacity"
    } else {
        "Capacity"
    };
    let total = field(&format!("{prefix}Size"));
    let used = field(&format!("{prefix}Used"));
    let remaining = field(&format!("{prefix}Remain"));
    if total.is_some_and(|size| size < 0.0) {
        return Err("个人资源包总额度无效".into());
    }
    let mut remaining = remaining
        .or_else(|| Some((total? - used?).max(0.0)))
        .ok_or_else(|| "余额资源包缺少明确的剩余量字段".to_string())?
        .max(0.0);
    if let Some(total) = total {
        remaining = remaining.min(total);
    }
    // Periodic packs can report a stale remain; their explicit used counter takes precedence.
    if prefix == "CycleCapacity" {
        if let (Some(size), Some(used)) = (total, used) {
            remaining = remaining.min((size - used.max(0.0)).max(0.0));
        }
    }
    let used = if prefix == "CycleCapacity" {
        total.map(|size| used.unwrap_or(0.0).max(size - remaining).max(0.0))
    } else {
        used.map(|value| value.max(0.0))
            .or_else(|| total.map(|size| (size - remaining).max(0.0)))
    };
    Ok(BalanceObservation {
        remaining: Some(remaining),
        total,
        used,
        unlimited: false,
    })
}

fn personal_page(response: &Value) -> Result<(BalanceObservation, Option<usize>), String> {
    let data = unwrap_data(response)?;
    let resource = data
        .pointer("/Response/Data")
        .ok_or_else(|| "余额响应缺少 Response.Data 资源数据".to_string())?;
    let accounts = resource
        .get("Accounts")
        .ok_or_else(|| "余额响应缺少 Accounts 资源包列表".to_string())?;
    let count = resource
        .get("TotalCount")
        .and_then(number)
        .filter(|count| *count >= 0.0)
        .map(|count| count as usize);
    let packages: &[Value] = if accounts.is_null() {
        &[]
    } else {
        accounts
            .as_array()
            .ok_or_else(|| "余额资源包列表格式无效".to_string())?
    };
    if packages.is_empty() && count.is_some_and(|count| count > 0) {
        return Err("余额资源包列表与总数不一致".into());
    }
    let mut result = BalanceObservation {
        remaining: Some(0.0),
        total: Some(0.0),
        used: Some(0.0),
        unlimited: false,
    };
    for package in packages {
        let balance = package_balance(package)?;
        result.remaining = Some(result.remaining.unwrap_or(0.0) + balance.remaining.unwrap_or(0.0));
        result.total = result
            .total
            .zip(balance.total)
            .map(|(sum, value)| sum + value);
        result.used = result
            .used
            .zip(balance.used)
            .map(|(sum, value)| sum + value);
    }
    Ok((result, count))
}

fn enterprise_balance(response: &Value) -> Result<BalanceObservation, String> {
    let data = unwrap_data(response)?;
    let limit = data
        .get("limitNum")
        .or_else(|| data.get("limit_num"))
        .and_then(number)
        .ok_or_else(|| "企业余额响应缺少分配额度 limitNum".to_string())?;
    let used = data
        .get("credit")
        .or_else(|| data.get("used_num"))
        .and_then(number)
        .ok_or_else(|| "企业余额响应缺少已用额度 credit".to_string())?
        .max(0.0);
    if limit < 0.0 {
        return Ok(BalanceObservation {
            remaining: None,
            total: None,
            used: Some(used),
            unlimited: true,
        });
    }
    Ok(BalanceObservation {
        remaining: Some((limit - used).max(0.0)),
        total: Some(limit),
        used: Some(used),
        unlimited: false,
    })
}

pub(crate) fn upstream_user_id(account: &AccountConfig) -> Option<Cow<'_, str>> {
    if let Some(uid) = account.official_uid() {
        return Some(Cow::Borrowed(uid));
    }

    // Imported accounts keep a stable local key. JWT claims only supply an optional billing
    // header. The upstream service still authenticates the bearer token itself.
    let mut parts = account.token.trim().split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if header.is_empty() || signature.is_empty() || parts.next().is_some() {
        return None;
    }
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    ["uid", "user_id", "userId"].iter().find_map(|key| {
        let value = claims.get(*key)?;
        let uid = if let Some(id) = value.as_str() {
            id.trim().to_string()
        } else {
            value.as_u64()?.to_string()
        };
        AccountConfig::valid_workbuddy_uid(&uid).then_some(Cow::Owned(uid))
    })
}

/// Shared billing identity for balance queries and daily check-in. Local account keys
/// must never be forwarded as official user IDs.
pub(crate) fn billing_post(
    client: &reqwest::Client,
    url: &str,
    account: &AccountConfig,
    body: &Value,
) -> reqwest::RequestBuilder {
    let mut request = client
        .post(url)
        .timeout(std::time::Duration::from_secs(25))
        .bearer_auth(account.token.trim())
        .header("User-Agent", "WorkBuddy/5.5.4")
        .header("X-CodeBuddy-Request", "1")
        .header("Accept", "application/json")
        .header(
            "Accept-Language",
            if account.realm == "global" {
                "en-US,en;q=0.9"
            } else {
                "zh-CN,zh;q=0.9"
            },
        )
        .json(body);
    if let Some(user_id) = upstream_user_id(account) {
        request = request.header("X-User-Id", user_id.as_ref());
    }
    if !account.enterprise_id.trim().is_empty() {
        request = request
            .header("X-Enterprise-Id", account.enterprise_id.trim())
            .header("X-Tenant-Id", account.enterprise_id.trim());
    }
    if !account.domain.trim().is_empty() {
        request = request.header("X-Domain", account.domain.trim());
    }
    request
}

async fn billing_request(
    client: &reqwest::Client,
    base: &str,
    paths: &[&str],
    account: &AccountConfig,
    body: &Value,
) -> Result<Value, String> {
    for (index, path) in paths.iter().enumerate() {
        let response = billing_post(client, &format!("{base}{path}"), account, body)
            .send()
            .await
            .map_err(|error| format!("余额请求失败: {error}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && index + 1 < paths.len() {
            continue;
        }
        if !status.is_success() {
            return Err(format!("余额查询失败（HTTP {status}）"));
        }
        return response
            .json()
            .await
            .map_err(|_| "余额接口返回了无效 JSON".to_string());
    }
    Err("余额接口不存在".into())
}

pub async fn fetch(
    client: &reqwest::Client,
    configured_base: &str,
    account: &AccountConfig,
) -> Result<BalanceObservation, String> {
    if account.token.trim().is_empty() {
        return Err("账号没有可用 Token，无法查询实际余额".into());
    }
    let base = billing_base(configured_base, &account.realm)?;
    let paths = resource_paths(account);
    if !account.enterprise_id.trim().is_empty() {
        let response = billing_request(client, &base, &paths, account, &json!({})).await?;
        return enterprise_balance(&response);
    }
    let now = crate::reqlog::now_ms();
    let mut body = json!({
        "PageNumber": 1, "PageSize": 100, "ProductCode": "p_tcaca", "Status": [0,3],
        "PackageEndTimeRangeBegin": beijing_timestamp(now),
        "PackageEndTimeRangeEnd": beijing_timestamp(now + 101 * 365 * 86_400_000)
    });
    let response = billing_request(client, &base, &paths, account, &body).await?;
    let (mut result, count) = personal_page(&response)?;
    let pages = count.unwrap_or(0).div_ceil(100);
    if pages > 100 {
        return Err("余额资源包数量异常，未返回不完整统计".into());
    }
    for page in 2..=pages {
        body["PageNumber"] = json!(page);
        let response = billing_request(client, &base, &paths, account, &body).await?;
        let (balance, _) = personal_page(&response)?;
        result.remaining = result
            .remaining
            .zip(balance.remaining)
            .map(|(sum, value)| sum + value);
        result.total = result
            .total
            .zip(balance.total)
            .map(|(sum, value)| sum + value);
        result.used = result
            .used
            .zip(balance.used)
            .map(|(sum, value)| sum + value);
    }
    Ok(result)
}

fn beijing_timestamp(milliseconds: i64) -> String {
    let seconds = milliseconds.div_euclid(1000) + 8 * 3600;
    let z = seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let time = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account(realm: &str, enterprise: &str) -> AccountConfig {
        serde_json::from_value(json!({"uid":"test-user","nickname":"test","token":"test-token","realm":realm,"enterprise_id":enterprise,"enabled":true,"paused":false})).unwrap()
    }
    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }
    #[tokio::test]
    async fn personal_fetch_uses_billing_post_and_exact_resource_fields() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let app = Router::new().route("/v2/billing/meter/get-user-resource", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer test-token");
            assert_eq!(headers["x-user-id"], "test-user");
            assert_eq!(headers["x-codebuddy-request"], "1");
            assert_eq!(body["ProductCode"], "p_tcaca");
            assert_eq!(body["PageNumber"], 1);
            Json(json!({"code":0,"data":{"Response":{"Data":{"TotalCount":1,"Accounts":[{"CapacitySize":2000,"CapacityRemain":1200,"CapacityUsed":800}]}}}}))
        }));
        let base = serve(app).await;
        let result = fetch(&reqwest::Client::new(), &base, &account("cn", ""))
            .await
            .unwrap();
        assert_eq!(result.remaining, Some(1200.0));
        assert_eq!(result.total, Some(2000.0));
        assert_eq!(result.used, Some(800.0));
    }

    #[tokio::test]
    async fn imported_account_uses_jwt_user_id_or_omits_the_header() {
        use axum::{http::HeaderMap, routing::post, Json, Router};

        let app = Router::new().route(
            "/v2/billing/meter/get-user-resource",
            post(|headers: HeaderMap| async move {
                match headers["authorization"].to_str().unwrap() {
                    "Bearer opaque-token" => assert!(!headers.contains_key("x-user-id")),
                    "Bearer repaired-token" => assert_eq!(headers["x-user-id"], "official-uid"),
                    _ => assert_eq!(headers["x-user-id"], "123456789"),
                }
                Json(json!({"code":0,"data":{"Response":{"Data":{"Accounts":[]}}}}))
            }),
        );
        let base = serve(app).await;
        let client = reqwest::Client::new();
        let mut imported = account("cn", "");
        imported.uid = "acct-1762329600000".into();
        let payload =
            general_purpose::URL_SAFE_NO_PAD.encode(br#"{"uid":123456789,"sub":"other-claim"}"#);
        imported.token = format!("e30.{payload}.signature");
        assert!(fetch(&client, &base, &imported).await.is_ok());
        assert_eq!(imported.uid, "acct-1762329600000");
        imported.uid = "local-account".into();
        imported.token = "opaque-token".into();
        assert!(fetch(&client, &base, &imported).await.is_ok());
        imported.token = "repaired-token".into();
        imported.workbuddy_uid = Some("official-uid".into());
        assert!(fetch(&client, &base, &imported).await.is_ok());
    }
    #[tokio::test]
    async fn global_fallback_and_enterprise_headers_follow_account_realm() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::post,
            Json, Router,
        };
        let app = Router::new()
            .route("/billing/meter/get-user-resource", post(|| async { StatusCode::NOT_FOUND }))
            .route("/v2/billing/meter/get-user-resource", post(|| async { Json(json!({"code":0,"data":{"Response":{"Data":{"Accounts":[{"CapacitySize":100,"CapacityRemain":25}]}}}})) }))
            .route("/v2/billing/meter/get-enterprise-user-usage", post(|headers: HeaderMap| async move {
                assert_eq!(headers["x-enterprise-id"], "test-enterprise");
                assert_eq!(headers["x-tenant-id"], "test-enterprise");
                Json(json!({"code":0,"data":{"credit":12.25,"limitNum":100}}))
            }));
        let base = serve(app).await;
        let client = reqwest::Client::new();
        let global = fetch(&client, &base, &account("global", "")).await.unwrap();
        assert_eq!(global.remaining, Some(25.0));
        let enterprise = fetch(&client, &base, &account("cn", "test-enterprise"))
            .await
            .unwrap();
        assert_eq!(enterprise.remaining, Some(87.75));
    }
    #[tokio::test]
    async fn pagination_sums_all_resource_packages_without_using_total_dosage() {
        use axum::{routing::post, Json, Router};
        let app = Router::new().route("/v2/billing/meter/get-user-resource", post(|Json(body): Json<Value>| async move {
            let packages = if body["PageNumber"] == 1 {
                vec![json!({"CapacitySize":10,"CapacityRemain":4,"CapacityUsed":6}); 100]
            } else {
                assert_eq!(body["PageNumber"], 2);
                vec![json!({"CapacitySize":50,"CapacityRemain":12,"CapacityUsed":38})]
            };
            Json(json!({"code":0,"data":{"Response":{"Data":{"TotalCount":101,"TotalDosage":999999,"Accounts":packages}}}}))
        }));
        let base = serve(app).await;
        let balance = fetch(&reqwest::Client::new(), &base, &account("cn", ""))
            .await
            .unwrap();
        assert_eq!(balance.remaining, Some(412.0));
        assert_eq!(balance.total, Some(1050.0));
        assert_eq!(balance.used, Some(638.0));
    }
    #[test]
    fn personal_remaining_is_not_total_quota_or_usage() {
        let (balance, _) = personal_page(&json!({"code":0,"data":{"credit":99999,"quota":55555,"Response":{"Data":{"TotalCount":2,"TotalDosage":3000,"Accounts":[
            {"CapacitySize":2000,"CapacityRemain":1200,"CapacityUsed":800,"CycleCapacitySize":2000,"CycleCapacityRemain":1000,"CycleCapacityUsed":1000},
            {"CapacitySize":1000,"CapacityRemain":300,"CapacityUsed":700}
        ]}}}})).unwrap();
        assert_eq!(balance.remaining, Some(1300.0));
        assert_eq!(balance.total, Some(3000.0));
        assert_eq!(balance.used, Some(1700.0));
    }
    #[test]
    fn zero_empty_and_invalid_data_remain_distinct() {
        let (empty, _) =
            personal_page(&json!({"code":0,"data":{"Response":{"Data":{"Accounts":null}}}}))
                .unwrap();
        assert_eq!(empty.remaining, Some(0.0));
        assert!(personal_page(&json!({"code":0,"data":{"balance":0,"quota":1000}})).is_err());
        assert!(
            personal_page(&json!({"code":10001,"data":{"Response":{"Data":{"Accounts":[]}}}}))
                .is_err()
        );
        assert!(package_balance(&json!({"credit":900,"total_quota":1000})).is_err());
    }
    #[test]
    fn cycle_remaining_is_bounded_by_size_and_used() {
        let balance = package_balance(
            &json!({"CycleCapacitySize":100,"CycleCapacityRemain":120,"CycleCapacityUsed":30}),
        )
        .unwrap();
        assert_eq!(balance.remaining, Some(70.0));
        let zero = package_balance(
            &json!({"CycleCapacitySize":100,"CycleCapacityRemain":-50,"CycleCapacityUsed":150}),
        )
        .unwrap();
        assert_eq!(zero.remaining, Some(0.0));
        let stale_used = package_balance(
            &json!({"CycleCapacitySize":100,"CycleCapacityRemain":70,"CycleCapacityUsed":20}),
        )
        .unwrap();
        assert_eq!(stale_used.used, Some(30.0));
    }
    #[test]
    fn enterprise_credit_is_used_and_unlimited_is_not_fake_balance() {
        let balance =
            enterprise_balance(&json!({"code":0,"data":{"credit":812.45,"limitNum":2000}}))
                .unwrap();
        assert_eq!(balance.remaining, Some(1187.55));
        assert_eq!(balance.used, Some(812.45));
        assert_eq!(balance.total, Some(2000.0));
        let unlimited =
            enterprise_balance(&json!({"code":0,"data":{"used_num":15,"limit_num":-1}})).unwrap();
        assert!(unlimited.unlimited);
        assert_eq!(unlimited.remaining, None);
        assert!(enterprise_balance(&json!({"code":0,"data":{"credit":50}})).is_err());
    }
    #[test]
    fn routing_uses_the_billing_host_for_each_realm() {
        assert_eq!(
            billing_base("https://www.workbuddy.cn", "cn").unwrap(),
            "https://www.codebuddy.cn"
        );
        assert_eq!(
            billing_base("https://www.workbuddy.cn", "global").unwrap(),
            "https://www.workbuddy.ai"
        );
        assert_eq!(
            billing_base("http://127.0.0.1:4567", "cn").unwrap(),
            "http://127.0.0.1:4567"
        );
        assert_eq!(beijing_timestamp(0), "1970-01-01 08:00:00");
    }
}
