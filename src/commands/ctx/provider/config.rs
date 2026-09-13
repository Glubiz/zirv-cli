use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::credential::CredentialRef;
use super::{
    AccountId, BillingClass, BillingPoolId, EndpointId, ProviderId, RouteId, Support, provider,
};
use crate::commands::ctx::CtxResult;

pub const NATIVE_CONFIG_FILE: &str = "native.toml";
pub const NATIVE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    pub schema: u32,
    #[serde(rename = "endpoint")]
    pub endpoints: BTreeMap<EndpointId, EndpointConfig>,
    #[serde(rename = "account")]
    pub accounts: BTreeMap<AccountId, AccountConfig>,
    #[serde(rename = "route")]
    pub routes: BTreeMap<RouteId, RouteConfig>,
    pub roles: BTreeMap<String, RouteId>,
    pub policy: NativePolicy,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub provider: ProviderId,
    pub base_url: Option<String>,
    pub vendor: Option<String>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            provider: ProviderId::new("missing").expect("static provider id"),
            base_url: None,
            vendor: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    pub provider: ProviderId,
    pub credential: Option<CredentialRef>,
    pub billing: BillingClass,
    pub pool: Option<BillingPoolId>,
    /// Required, and only meaningful, for the `google-vertex` profile: Vertex
    /// AI addresses a model by project and location, not by a bare API key.
    pub project: Option<String>,
    pub location: Option<String>,
    /// Required, and only meaningful, for `aws-bedrock`: SigV4 signs for one
    /// region, and the signature is not portable to another one.
    pub region: Option<String>,
    /// Required, and only meaningful, for `azure-openai`: the data-plane
    /// `api-version` an Azure resource is pinned to.
    pub api_version: Option<String>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            provider: ProviderId::new("missing").expect("static provider id"),
            credential: None,
            billing: BillingClass::Api,
            pool: None,
            project: None,
            location: None,
            region: None,
            api_version: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteConfig {
    pub account: AccountId,
    pub endpoint: Option<EndpointId>,
    pub model: String,
    /// Required, and only meaningful, for `azure-openai`: an Azure route is
    /// addressed by deployment id, and the model a deployment serves is an
    /// account fact zirv cannot infer from the id.
    pub deployment: Option<String>,
    /// Provider-native request options, validated against the route
    /// profile's typed allow-list (`profiles::validate_extensions`).
    pub extensions: BTreeMap<String, toml::Value>,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            account: AccountId::new("missing").expect("static account id"),
            endpoint: None,
            model: String::new(),
            deployment: None,
            extensions: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativePolicy {
    pub allowed_routes: Option<BTreeSet<RouteId>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEndpoint {
    pub id: EndpointId,
    pub provider: ProviderId,
    pub base_url: String,
    pub vendor: String,
    pub implicit: bool,
}

impl NativeConfig {
    pub fn operator_path(home: &Path) -> PathBuf {
        home.join(crate::utils::SCRIPT_DIR_NAME)
            .join(NATIVE_CONFIG_FILE)
    }

    pub fn repo_path(repo: &Path) -> PathBuf {
        repo.join(crate::utils::SCRIPT_DIR_NAME)
            .join(NATIVE_CONFIG_FILE)
    }

    pub fn load(home: &Path, repo: &Path) -> CtxResult<Option<Self>> {
        let operator_path = Self::operator_path(home);
        let text = match std::fs::read_to_string(&operator_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("{}: {error}", operator_path.display()).into());
            }
        };
        let mut config: Self = toml::from_str(&text)
            .map_err(|error| format!("{}: {error}", operator_path.display()))?;
        validate_schema(config.schema, &operator_path)?;

        if !crate::utils::repo_is_home(repo) {
            let repo_path = Self::repo_path(repo);
            match std::fs::read_to_string(&repo_path) {
                Ok(repo_text) => {
                    let table: toml::Table = toml::from_str(&repo_text)
                        .map_err(|error| format!("{}: {error}", repo_path.display()))?;
                    reject_untrusted_keys(&table, &repo_path)?;
                    let repo_cfg: RepoConfig = toml::from_str(&repo_text)
                        .map_err(|error| format!("{}: {error}", repo_path.display()))?;
                    validate_schema(repo_cfg.schema, &repo_path)?;
                    if let Some(repo_allowed) = repo_cfg.policy.allowed_routes {
                        let operator_allowed = config
                            .policy
                            .allowed_routes
                            .clone()
                            .unwrap_or_else(|| config.routes.keys().cloned().collect());
                        config.policy.allowed_routes = Some(
                            operator_allowed
                                .intersection(&repo_allowed)
                                .cloned()
                                .collect(),
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", repo_path.display()).into()),
            }
        }

        if config.policy.allowed_routes.is_none() {
            config.policy.allowed_routes = Some(config.routes.keys().cloned().collect());
        }
        config.validate(&operator_path)?;
        Ok(Some(config))
    }

    pub fn allowed_routes(&self) -> &BTreeSet<RouteId> {
        self.policy
            .allowed_routes
            .as_ref()
            .expect("validated config always resolves allowed_routes")
    }

    pub fn account_pool(&self, id: &AccountId) -> BillingPoolId {
        self.accounts
            .get(id)
            .and_then(|account| account.pool.clone())
            .unwrap_or_else(|| BillingPoolId::new(id.as_ref()).expect("account ids are pool ids"))
    }

    pub fn effective_endpoints(&self) -> BTreeMap<EndpointId, ResolvedEndpoint> {
        let mut endpoints = BTreeMap::new();
        for spec in super::providers() {
            if let (Some(base_url), Some(vendor)) = (spec.default_base_url, spec.vendor) {
                let id = EndpointId::new(spec.id).expect("static provider id");
                endpoints.insert(
                    id.clone(),
                    ResolvedEndpoint {
                        id,
                        provider: ProviderId::new(spec.id).expect("static provider id"),
                        base_url: base_url.to_string(),
                        vendor: vendor.to_string(),
                        implicit: true,
                    },
                );
            }
        }
        for (id, endpoint) in &self.endpoints {
            let Some(spec) = provider(endpoint.provider.as_ref()) else {
                continue;
            };
            let Some(vendor) = endpoint
                .vendor
                .clone()
                .or_else(|| spec.vendor.map(str::to_string))
            else {
                continue;
            };
            // A vendor with a documented base URL does not need the operator
            // to retype it; the profile registry is where that fact lives.
            let Some(base_url) = endpoint.base_url.clone().or_else(|| {
                spec.default_base_url.map(str::to_string).or_else(|| {
                    super::profiles::profile_for(spec.id, &vendor)
                        .and_then(|profile| profile.base_url.default_url())
                        .map(str::to_string)
                })
            }) else {
                continue;
            };
            endpoints.insert(
                id.clone(),
                ResolvedEndpoint {
                    id: id.clone(),
                    provider: endpoint.provider.clone(),
                    base_url,
                    vendor,
                    implicit: false,
                },
            );
        }
        endpoints
    }

    pub fn route_endpoint(&self, route: &RouteConfig) -> Option<EndpointId> {
        route.endpoint.clone().or_else(|| {
            self.accounts
                .get(&route.account)
                .map(|account| EndpointId::new(account.provider.as_ref()).expect("provider id"))
        })
    }

    fn validate(&self, path: &Path) -> CtxResult<()> {
        for (id, endpoint) in &self.endpoints {
            let key = format!("endpoint.{id}");
            let spec = provider(endpoint.provider.as_ref()).ok_or_else(|| {
                format!(
                    "{}: `{key}.provider` names unknown provider `{}`",
                    path.display(),
                    endpoint.provider
                )
            })?;
            if let Some(base_url) = endpoint.base_url.as_deref() {
                super::super::config::validate_endpoint_base_url(
                    &format!("{key}.base_url"),
                    base_url,
                )
                .map_err(|error| format!("{}: {error}", path.display()))?;
            }
            if spec.vendor.is_none() {
                let vendor = endpoint.vendor.as_deref().ok_or_else(|| {
                    format!(
                        "{}: `{key}.vendor` is required for provider `{}`",
                        path.display(),
                        spec.id
                    )
                })?;
                ProviderId::new(vendor)
                    .map_err(|error| format!("{}: `{key}.vendor`: {error}", path.display()))?;
                let profile = super::profiles::profile_for(spec.id, vendor).ok_or_else(|| {
                    format!(
                        "{}: `{key}` has no route profile for vendor `{vendor}` on provider `{}`",
                        path.display(),
                        spec.id
                    )
                })?;
                if endpoint.base_url.is_none() && profile.base_url.default_url().is_none() {
                    return Err(format!(
                        "{}: `{key}.base_url` is required because route profile `{}` has no \
                         documented base URL",
                        path.display(),
                        profile.id
                    )
                    .into());
                }
                if let Some(base_url) = endpoint.base_url.as_deref()
                    && base_url.starts_with("http://")
                    && !profile.allows_plain_http()
                {
                    return Err(format!(
                        "{}: `{key}.base_url` is plaintext http, which route profile `{}` does \
                         not allow; only a local runtime may be reached without TLS",
                        path.display(),
                        profile.id
                    )
                    .into());
                }
                if let Some(base_url) = endpoint.base_url.as_deref()
                    && profile.allows_plain_http()
                    && base_url.starts_with("http://")
                    && !super::probe::is_local_http_host(base_url)
                {
                    return Err(format!(
                        "{}: `{key}.base_url` reaches a public host over plaintext http; a local \
                         runtime must be on a loopback or private address",
                        path.display()
                    )
                    .into());
                }
            } else if endpoint.vendor.is_some() {
                return Err(format!(
                    "{}: `{key}.vendor` is forbidden because provider `{}` fixes vendor `{}`",
                    path.display(),
                    spec.id,
                    spec.vendor.unwrap_or("")
                )
                .into());
            }
        }

        for (id, account) in &self.accounts {
            let Some(spec) = provider(account.provider.as_ref()) else {
                return Err(format!(
                    "{}: `account.{id}.provider` names unknown provider `{}`",
                    path.display(),
                    account.provider
                )
                .into());
            };
            if account.billing == BillingClass::Api
                && spec.id != "openai-compatible"
                && account.credential.is_none()
            {
                return Err(format!(
                    "{}: `account.{id}.credential` is required for provider `{}`",
                    path.display(),
                    account.provider
                )
                .into());
            }
            // Per-provider identity fields. Each one is required by exactly
            // one provider and forbidden everywhere else, so a Vertex project
            // can never be read as a Bedrock region or an Azure api-version.
            let identity: [(&str, &str, &Option<String>); 4] = [
                ("google-vertex", "project", &account.project),
                ("google-vertex", "location", &account.location),
                ("aws-bedrock", "region", &account.region),
                ("azure-openai", "api_version", &account.api_version),
            ];
            for (owner, field, value) in identity {
                if spec.id == owner {
                    if value.as_deref().is_none_or(str::is_empty) {
                        return Err(format!(
                            "{}: `account.{id}.{field}` is required for provider `{owner}`",
                            path.display()
                        )
                        .into());
                    }
                } else if value.is_some() {
                    return Err(format!(
                        "{}: `account.{id}.{field}` is forbidden because provider `{}` is not {owner}",
                        path.display(),
                        account.provider
                    )
                    .into());
                }
            }
        }

        let endpoints = self.effective_endpoints();
        for (id, route) in &self.routes {
            let account = self.accounts.get(&route.account).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.account` references undeclared account `{}`",
                    path.display(),
                    route.account
                )
            })?;
            let spec = provider(account.provider.as_ref()).expect("account provider validated");
            if let Support::Planned(tracking) = spec.support {
                return Err(format!(
                    "{}: `route.{id}` uses provider `{}` which is not yet supported natively; tracked as {tracking}",
                    path.display(), spec.id
                ).into());
            }
            if route.endpoint.is_none() && spec.default_base_url.is_none() {
                return Err(format!(
                    "{}: `route.{id}.endpoint` is required because provider `{}` has no default endpoint",
                    path.display(), account.provider
                )
                .into());
            }
            let endpoint_id = self.route_endpoint(route).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.endpoint` is required because provider `{}` has no default endpoint",
                    path.display(), account.provider
                )
            })?;
            let endpoint = endpoints.get(&endpoint_id).ok_or_else(|| {
                format!(
                    "{}: `route.{id}.endpoint` references undeclared endpoint `{endpoint_id}`",
                    path.display()
                )
            })?;
            if endpoint.provider != account.provider {
                return Err(format!(
                    "{}: `route.{id}.endpoint` provider `{}` does not match account provider `{}`",
                    path.display(),
                    endpoint.provider,
                    account.provider
                )
                .into());
            }
            if let Err(vendor_prefix) =
                super::inventory::nonempty_model_name(&endpoint.vendor, &route.model)
            {
                let problem = vendor_prefix.map_or_else(
                    || "is required".to_string(),
                    |_| format!("must name a model after the `{}/` prefix", endpoint.vendor),
                );
                return Err(format!("{}: `route.{id}.model` {problem}", path.display()).into());
            }
            super::inventory::resolve_model(id, &endpoint_id, &endpoint.vendor, &route.model)
                .map_err(|error| format!("{}: `route.{id}.model`: {error}", path.display()))?;

            // Every accessible route binds to a profile. An unbound route is
            // refused here rather than sent to a guessed endpoint.
            let profile =
                super::profiles::profile_for(spec.id, &endpoint.vendor).ok_or_else(|| {
                    format!(
                        "{}: `route.{id}` has no route profile for vendor `{}` on provider `{}`",
                        path.display(),
                        endpoint.vendor,
                        spec.id
                    )
                })?;
            match profile.support {
                Support::Native => {}
                Support::Planned(tracking) => {
                    return Err(format!(
                        "{}: `route.{id}` needs route profile `{}`, whose adapter is not \
                         implemented yet; tracked as {tracking}",
                        path.display(),
                        profile.id
                    )
                    .into());
                }
                Support::LegacyOnly(reason) => {
                    return Err(format!(
                        "{}: `route.{id}` cannot be native: {reason}. Run that model through its \
                         coding-harness backend instead.",
                        path.display()
                    )
                    .into());
                }
            }
            if profile.credential == super::profiles::CredentialClass::LocalNone
                && account.credential.is_some()
            {
                return Err(format!(
                    "{}: `account.{}.credential` is set but route profile `{}` is a local \
                     runtime that takes no credential; remove it rather than sending a secret to \
                     a local server",
                    path.display(),
                    route.account,
                    profile.id
                )
                .into());
            }
            if !profile.credential.is_optional()
                && account.credential.is_none()
                && spec.default_credential_env.is_empty()
                && profile.credential_env.is_empty()
            {
                return Err(format!(
                    "{}: `account.{}.credential` is required by route profile `{}`",
                    path.display(),
                    route.account,
                    profile.id
                )
                .into());
            }
            if spec.id == "azure-openai" {
                if route.deployment.as_deref().is_none_or(str::is_empty) {
                    return Err(format!(
                        "{}: `route.{id}.deployment` is required for provider `azure-openai`",
                        path.display()
                    )
                    .into());
                }
            } else if route.deployment.is_some() {
                return Err(format!(
                    "{}: `route.{id}.deployment` is forbidden because provider `{}` is not azure-openai",
                    path.display(),
                    account.provider
                )
                .into());
            }
            super::profiles::validate_extensions(profile, &route.extensions)
                .map_err(|error| format!("{}: `route.{id}.extensions`: {error}", path.display()))?;
        }

        for (role, route) in &self.roles {
            if !self.routes.contains_key(route) {
                return Err(format!(
                    "{}: `roles.{role}` references undeclared route `{route}`",
                    path.display()
                )
                .into());
            }
            if !self.allowed_routes().contains(route) {
                return Err(format!(
                    "{}: `roles.{role}` references route `{route}` outside effective `policy.allowed_routes`",
                    path.display()
                ).into());
            }
        }
        Ok(())
    }
}

fn validate_schema(schema: u32, path: &Path) -> CtxResult<()> {
    if schema == 0 {
        return Err(format!("{}: `schema` is required", path.display()).into());
    }
    if schema > NATIVE_SCHEMA {
        return Err(format!(
            "{}: native.toml schema {schema} is newer than this zirv (supports {NATIVE_SCHEMA}); upgrade zirv",
            path.display()
        ).into());
    }
    if schema != NATIVE_SCHEMA {
        return Err(format!(
            "{}: unsupported native.toml schema {schema}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RepoConfig {
    schema: u32,
    policy: NativePolicy,
}

fn reject_untrusted_keys(table: &toml::Table, path: &Path) -> CtxResult<()> {
    for (key, value) in table {
        match key.as_str() {
            "schema" => {}
            "policy" => {
                if let Some(policy) = value.as_table() {
                    for policy_key in policy.keys() {
                        if policy_key != "allowed_routes" {
                            return Err(repo_forbidden(path, &format!("policy.{policy_key}")));
                        }
                    }
                }
            }
            "endpoint" | "account" | "route" | "roles" => {
                let dotted = value
                    .as_table()
                    .and_then(|nested| nested.keys().next())
                    .map_or_else(|| key.clone(), |nested| format!("{key}.{nested}"));
                return Err(repo_forbidden(path, &dotted));
            }
            other => return Err(repo_forbidden(path, other)),
        }
    }
    Ok(())
}

fn repo_forbidden(path: &Path, key: &str) -> Box<dyn std::error::Error> {
    format!(
        "{}: `{key}` may not be set by a repository config; set it in ~/.zirv/native.toml",
        path.display()
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::super::super::testenv::{HomeGuard, repo};
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn load_is_none_until_the_operator_opts_in() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        assert_eq!(NativeConfig::load(home.path(), repo.path()).unwrap(), None);
    }

    #[test]
    fn newer_schema_and_unknown_keys_are_actionable() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        write(&path, "schema = 2\n");
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("schema 2 is newer"),
            "got {error}"
        );
        write(&path, "schema = 1\nunknown = true\n");
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("unknown"), "got {error}");
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn repository_accounts_are_hard_refused_by_dotted_key() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(&NativeConfig::operator_path(home.path()), "schema = 1\n");
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema = 1\n[account.work]\nprovider = 'anthropic'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("`account.work`"), "got {error}");
        assert!(error.to_string().contains("~/.zirv/native.toml"));
    }

    #[test]
    fn repository_allowed_routes_intersects_and_never_widens() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n[route.a]\naccount='work'\nmodel='haiku'\n[route.b]\naccount='work'\nmodel='sonnet'\n[policy]\nallowed_routes=['a']\n",
        );
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema=1\n[policy]\nallowed_routes=['a','b','undeclared']\n",
        );
        let cfg = NativeConfig::load(home.path(), repo.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            cfg.allowed_routes()
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn narrowing_cannot_leave_a_bound_role_on_a_disallowed_route() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n[route.a]\naccount='work'\nmodel='haiku'\n[route.b]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='b'\n",
        );
        write(
            &NativeConfig::repo_path(repo.path()),
            "schema=1\n[policy]\nallowed_routes=['a']\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(error.to_string().contains("roles.worker"), "got {error}");
        assert!(
            error.to_string().contains("outside effective"),
            "got {error}"
        );
    }

    #[test]
    fn bedrock_routes_need_a_vendor_and_a_signing_region() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        let endpoint = "[endpoint.bedrock]\nprovider='aws-bedrock'\nbase_url='https://bedrock-runtime.us-east-1.amazonaws.com'\n";
        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='aws-bedrock'\ncredential='env:KEY'\nregion='us-east-1'\n[route.work]\naccount='work'\nendpoint='bedrock'\nmodel='claude-sonnet-5'\n"
            ),
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`endpoint.bedrock.vendor`"),
            "got {error}"
        );

        let endpoint = format!("{endpoint}vendor='anthropic'\n");
        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='aws-bedrock'\ncredential='env:KEY'\n[route.work]\naccount='work'\nendpoint='bedrock'\nmodel='claude-sonnet-5'\n"
            ),
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`account.work.region`"),
            "got {error}"
        );

        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='aws-bedrock'\ncredential='env:KEY'\nregion='us-east-1'\n[route.work]\naccount='work'\nendpoint='bedrock'\nmodel='claude-sonnet-5'\n"
            ),
        );
        let cfg = NativeConfig::load(home.path(), repo.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            cfg.accounts
                .get(&AccountId::new("work").unwrap())
                .unwrap()
                .region
                .as_deref(),
            Some("us-east-1")
        );
    }

    #[test]
    fn azure_routes_are_addressed_by_deployment_and_api_version() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        let endpoint = "[endpoint.azure]\nprovider='azure-openai'\nbase_url='https://contoso.openai.azure.com'\n";
        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='azure-openai'\ncredential='env:KEY'\n[route.work]\naccount='work'\nendpoint='azure'\nmodel='gpt-5.6-sol'\n"
            ),
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`account.work.api_version`"),
            "got {error}"
        );

        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='azure-openai'\ncredential='env:KEY'\napi_version='2026-05-01'\n[route.work]\naccount='work'\nendpoint='azure'\nmodel='gpt-5.6-sol'\n"
            ),
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`route.work.deployment`"),
            "got {error}"
        );

        write(
            &path,
            &format!(
                "schema=1\n{endpoint}[account.work]\nprovider='azure-openai'\ncredential='env:KEY'\napi_version='2026-05-01'\n[route.work]\naccount='work'\nendpoint='azure'\nmodel='gpt-5.6-sol'\ndeployment='sol-prod'\n"
            ),
        );
        assert!(
            NativeConfig::load(home.path(), repo.path())
                .unwrap()
                .is_some()
        );

        // A deployment on a non-Azure route is a configuration error, not a
        // silently ignored key.
        write(
            &path,
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n[route.work]\naccount='work'\nmodel='sonnet'\ndeployment='sol-prod'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("is forbidden because provider"),
            "got {error}"
        );
    }

    #[test]
    fn a_broker_subscription_vendor_is_refused_with_its_upstream_reason() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.copilot]\nprovider='openai-compatible'\nbase_url='https://api.example.invalid'\nvendor='copilot'\n[account.copilot]\nprovider='openai-compatible'\ncredential='env:KEY'\n[route.copilot]\naccount='copilot'\nendpoint='copilot'\nmodel='gpt-5.6-sol'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be native"), "got {error}");
        assert!(error.contains("Copilot"), "got {error}");
        assert!(error.contains("coding-harness backend"), "got {error}");
    }

    #[test]
    fn a_documented_vendor_base_url_does_not_have_to_be_retyped() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.deepseek]\nprovider='openai-compatible'\nvendor='deepseek'\n[account.deepseek]\nprovider='openai-compatible'\ncredential='env:DEEPSEEK_API_KEY'\n[route.reason]\naccount='deepseek'\nendpoint='deepseek'\nmodel='deepseek-v4-pro'\n[route.reason.extensions]\ntemperature=0.2\n",
        );
        let cfg = NativeConfig::load(home.path(), repo.path())
            .unwrap()
            .unwrap();
        let endpoint = cfg.effective_endpoints();
        assert_eq!(
            endpoint
                .get(&EndpointId::new("deepseek").unwrap())
                .unwrap()
                .base_url,
            "https://api.deepseek.com"
        );
    }

    #[test]
    fn route_extensions_are_validated_against_the_bound_profile() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.deepseek]\nprovider='openai-compatible'\nvendor='deepseek'\n[account.deepseek]\nprovider='openai-compatible'\ncredential='env:KEY'\n[route.reason]\naccount='deepseek'\nendpoint='deepseek'\nmodel='deepseek-v4-pro'\n[route.reason.extensions]\nenable_thinking=true\n",
        );
        let error = NativeConfig::load(home.path(), repo.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`route.reason.extensions`"), "got {error}");
        assert!(error.contains("not an extension of profile"), "got {error}");
    }

    #[test]
    fn plaintext_http_is_only_for_a_local_runtime_on_a_local_host() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        write(
            &path,
            "schema=1\n[endpoint.remote]\nprovider='openai-compatible'\nbase_url='http://api.deepseek.com'\nvendor='deepseek'\n[account.a]\nprovider='openai-compatible'\ncredential='env:KEY'\n[route.a]\naccount='a'\nendpoint='remote'\nmodel='deepseek-v4-pro'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not allow"), "got {error}");

        write(
            &path,
            "schema=1\n[endpoint.remote]\nprovider='openai-compatible'\nbase_url='http://models.example.com:11434'\nvendor='ollama'\n[account.a]\nprovider='openai-compatible'\n[route.a]\naccount='a'\nendpoint='remote'\nmodel='qwen3'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("loopback or private"), "got {error}");

        write(
            &path,
            "schema=1\n[endpoint.lan]\nprovider='openai-compatible'\nbase_url='http://192.168.1.9:11434'\nvendor='ollama'\n[account.a]\nprovider='openai-compatible'\n[route.a]\naccount='a'\nendpoint='lan'\nmodel='qwen3'\n",
        );
        assert!(
            NativeConfig::load(home.path(), repo.path())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_local_runtime_route_never_carries_a_credential() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.local]\nprovider='openai-compatible'\nvendor='ollama'\n[account.local]\nprovider='openai-compatible'\ncredential='env:KEY'\n[route.local]\naccount='local'\nendpoint='local'\nmodel='qwen3'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("takes no credential"), "got {error}");
    }

    #[test]
    fn google_vertex_requires_project_and_location() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        let path = NativeConfig::operator_path(home.path());
        write(
            &path,
            "schema=1\n[endpoint.vertex]\nprovider='google-vertex'\nbase_url='https://us-central1-aiplatform.googleapis.com'\n[account.work]\nprovider='google-vertex'\ncredential='env:VERTEX_TOKEN'\n[route.work]\naccount='work'\nendpoint='vertex'\nmodel='gemini-3.1-pro-preview'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`account.work.project`"),
            "got {error}"
        );

        write(
            &path,
            "schema=1\n[endpoint.vertex]\nprovider='google-vertex'\nbase_url='https://us-central1-aiplatform.googleapis.com'\n[account.work]\nprovider='google-vertex'\ncredential='env:VERTEX_TOKEN'\nproject='proj-1'\n[route.work]\naccount='work'\nendpoint='vertex'\nmodel='gemini-3.1-pro-preview'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`account.work.location`"),
            "got {error}"
        );

        write(
            &path,
            "schema=1\n[endpoint.vertex]\nprovider='google-vertex'\nbase_url='https://us-central1-aiplatform.googleapis.com'\n[account.work]\nprovider='google-vertex'\ncredential='env:VERTEX_TOKEN'\nproject='proj-1'\nlocation='us-central1'\n[route.work]\naccount='work'\nendpoint='vertex'\nmodel='gemini-3.1-pro-preview'\n",
        );
        let cfg = NativeConfig::load(home.path(), repo.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            cfg.accounts
                .get(&AccountId::new("work").unwrap())
                .unwrap()
                .project
                .as_deref(),
            Some("proj-1")
        );

        write(
            &path,
            "schema=1\n[account.plain]\nprovider='anthropic'\ncredential='env:KEY'\nproject='proj-1'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("forbidden because provider"),
            "got {error}"
        );
    }

    #[test]
    fn every_route_requires_a_nonempty_model() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.local]\nprovider='openai-compatible'\nbase_url='http://127.0.0.1:11434'\nvendor='ollama'\n[account.local]\nprovider='openai-compatible'\n[route.local]\naccount='local'\nendpoint='local'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error.to_string().contains("`route.local.model`"),
            "got {error}"
        );
    }

    #[test]
    fn vendor_prefix_must_be_followed_by_a_nonempty_model() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let repo = repo();
        write(
            &NativeConfig::operator_path(home.path()),
            "schema=1\n[endpoint.local]\nprovider='openai-compatible'\nbase_url='http://127.0.0.1:11434'\nvendor='ollama'\n[account.local]\nprovider='openai-compatible'\n[route.local]\naccount='local'\nendpoint='local'\nmodel='ollama/'\n",
        );
        let error = NativeConfig::load(home.path(), repo.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`route.local.model` must name a model after the `ollama/` prefix"),
            "got {error}"
        );

        let route = RouteId::new("local").unwrap();
        let endpoint = EndpointId::new("local").unwrap();
        assert!(
            super::super::inventory::resolve_model(&route, &endpoint, "ollama", "ollama/").is_err()
        );
    }
}
