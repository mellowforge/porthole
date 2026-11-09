use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Mysql,
    Sqlite,
    Sqlserver,
    Mongodb,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Local,
    Staging,
    Production,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", try_from = "ProfileWire")]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    pub environment: Environment,
    pub default_schema: Option<String>,
    pub connect_timeout_secs: u32,
    pub read_only: bool,
    pub startup_sql: Option<String>,
    /// MongoDB query tabs' statement time limit; absent for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_secs: Option<u32>,
    #[serde(flatten)]
    pub transport: Transport,
}

/// How a profile reaches its database.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "transport", rename_all = "camelCase")]
pub enum Transport {
    Network(NetworkTarget),
    File {
        #[serde(rename = "filePath")]
        file_path: String,
    },
    Uri {
        uri: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub ssl_mode: SslMode,
    #[serde(default)]
    pub ssl_root_cert: Option<String>,
    /// MongoDB's authentication database; absent for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_source: Option<String>,
}

impl ConnectionProfile {
    /// The server address of a network profile.
    pub fn network(&self) -> AppResult<&NetworkTarget> {
        match &self.transport {
            Transport::Network(n) => Ok(n),
            _ => Err(AppError::Connection("This connection has no server address.".into())),
        }
    }
}

/// The `transport` tag of a stored profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportTag {
    Network,
    File,
    Uri,
}

impl<'de> Deserialize<'de> for TransportTag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let tag = String::deserialize(d)?;
        match tag.as_str() {
            "network" => Ok(TransportTag::Network),
            "file" => Ok(TransportTag::File),
            "uri" => Ok(TransportTag::Uri),
            other => Err(serde::de::Error::custom(format!("unknown transport `{other}`"))),
        }
    }
}

/// The profile as stored or sent: today's flat fields, all optional, plus the
/// `transport` tag that older saved profiles lack.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileWire {
    id: String,
    name: String,
    kind: DbKind,
    environment: Environment,
    #[serde(default)]
    default_schema: Option<String>,
    connect_timeout_secs: u32,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    startup_sql: Option<String>,
    #[serde(default)]
    statement_timeout_secs: Option<u32>,
    #[serde(default)]
    transport: Option<TransportTag>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    database: Option<String>,
    ssl_mode: Option<SslMode>,
    #[serde(default)]
    ssl_root_cert: Option<String>,
    #[serde(default)]
    auth_source: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

impl TryFrom<ProfileWire> for ConnectionProfile {
    type Error = String;

    fn try_from(w: ProfileWire) -> Result<Self, String> {
        let tag = match w.transport {
            Some(t) => t,
            None if w.kind == DbKind::Sqlite => TransportTag::File,
            None => TransportTag::Network,
        };
        let transport = match tag {
            TransportTag::Network => {
                let need = |name: &str| format!("missing field `{name}`");
                Transport::Network(NetworkTarget {
                    host: w.host.ok_or_else(|| need("host"))?,
                    port: w.port.ok_or_else(|| need("port"))?,
                    user: w.user.ok_or_else(|| need("user"))?,
                    database: w.database.ok_or_else(|| need("database"))?,
                    ssl_mode: w.ssl_mode.ok_or_else(|| need("sslMode"))?,
                    ssl_root_cert: w.ssl_root_cert,
                    auth_source: w.auth_source,
                })
            }
            TransportTag::File => Transport::File { file_path: w.file_path.unwrap_or_default() },
            TransportTag::Uri => Transport::Uri { uri: w.uri.ok_or("missing field `uri`")? },
        };
        Ok(ConnectionProfile {
            id: w.id,
            name: w.name,
            kind: w.kind,
            environment: w.environment,
            default_schema: w.default_schema,
            connect_timeout_secs: w.connect_timeout_secs,
            read_only: w.read_only,
            startup_sql: w.startup_sql,
            statement_timeout_secs: w.statement_timeout_secs,
            transport,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FunctionInfo {
    pub name: String,
    pub signature: String,
    /// Absent for drivers whose routines are all functions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RoutineKind>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RoutineKind {
    Function,
    Procedure,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SchemaTree {
    pub tables: Vec<String>,
    pub views: Vec<String>,
    pub functions: Vec<FunctionInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    /// Short type name, e.g. `int4`.
    pub data_type: String,
    /// `format_type` output, used for casts.
    pub full_type: String,
    /// The type parameters are cast to: `format_type` without typmod, except
    /// `bpchar` for `char(n)` and `varbit` for `bit(n)` (bare `character`/`bit`
    /// mean length 1).
    pub cast_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_primary_key: bool,
    /// Identity `ALWAYS` or stored generated: the database fills it in.
    pub read_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
    Table,
    Partitioned,
    Foreign,
    View,
    Materialized,
}

/// Whether the grid may edit rows. Read-only connections are decided in the UI.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Editing {
    Allowed,
    NoPrimaryKey,
    View,
    FloatKey,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    /// INCLUDE columns (Postgres 11+); left out when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: TableRef,
    pub ref_columns: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TableStructure {
    pub columns: Vec<ColumnInfo>,
    /// Primary-key column names in key (index) order, INCLUDE columns excluded.
    pub primary_key: Vec<String>,
    pub relation: RelationKind,
    pub editing: Editing,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

/// Column name to new text value (`None` is NULL).
pub type CellValues = BTreeMap<String, Option<String>>;

/// The storage type of one cell, for databases whose cells carry their own type (SQLite).
/// Postgres never sends one: its column types say it all.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellType {
    #[serde(rename = "n")]
    Null,
    #[serde(rename = "i")]
    Integer,
    #[serde(rename = "r")]
    Real,
    #[serde(rename = "t")]
    Text,
    #[serde(rename = "b")]
    Blob,
    /// 64-bit integer (MongoDB).
    #[serde(rename = "l")]
    Long,
    /// Decimal128 (MongoDB).
    #[serde(rename = "m")]
    Decimal,
    #[serde(rename = "y")]
    Bool,
    /// ObjectId (MongoDB).
    #[serde(rename = "o")]
    ObjectId,
    #[serde(rename = "d")]
    Date,
    /// Document or array, as extended JSON (MongoDB).
    #[serde(rename = "j")]
    Json,
    /// Any other BSON type, as canonical extended JSON; not editable (MongoDB).
    #[serde(rename = "x")]
    Other,
}

impl CellType {
    /// The one-letter code used on the wire.
    pub fn code(self) -> char {
        match self {
            CellType::Null => 'n',
            CellType::Integer => 'i',
            CellType::Real => 'r',
            CellType::Text => 't',
            CellType::Blob => 'b',
            CellType::Long => 'l',
            CellType::Decimal => 'm',
            CellType::Bool => 'y',
            CellType::ObjectId => 'o',
            CellType::Date => 'd',
            CellType::Json => 'j',
            CellType::Other => 'x',
        }
    }

    pub fn from_code(c: char) -> Option<CellType> {
        match c {
            'n' => Some(CellType::Null),
            'i' => Some(CellType::Integer),
            'r' => Some(CellType::Real),
            't' => Some(CellType::Text),
            'b' => Some(CellType::Blob),
            'l' => Some(CellType::Long),
            'm' => Some(CellType::Decimal),
            'y' => Some(CellType::Bool),
            'o' => Some(CellType::ObjectId),
            'd' => Some(CellType::Date),
            'j' => Some(CellType::Json),
            'x' => Some(CellType::Other),
            _ => None,
        }
    }
}

/// One code per column, e.g. "itnb".
pub fn row_types(types: &[CellType]) -> String {
    types.iter().map(|t| t.code()).collect()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RowChange {
    /// `key` holds the original primary-key values, in primary-key order.
    Update {
        key: Vec<String>,
        values: CellValues,
        /// The original cell types of the key columns and the edited columns; sent only when
        /// the page had `cellTypes`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        original_types: BTreeMap<String, CellType>,
    },
    /// Only the columns the user set; empty means `DEFAULT VALUES`.
    Insert {
        values: CellValues,
    },
    Delete {
        key: Vec<String>,
        /// The original cell types of the key columns; sent only when the page had `cellTypes`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        original_types: BTreeMap<String, CellType>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSet {
    pub table: TableRef,
    pub changes: Vec<RowChange>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub applied: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

/// A table with the grid's filter and sort, for exporting every matching row.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TableQuery {
    pub table: TableRef,
    pub filter: Option<String>,
    pub sort: Option<SortSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PageRequest {
    pub table: TableRef,
    pub filter: Option<String>,
    pub sort: Option<SortSpec>,
    /// 0-based page index.
    pub page: u32,
    pub page_size: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum RowCount {
    Exact(u64),
    Estimated(u64),
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TablePage {
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Vec<String>,
    pub editing: Editing,
    pub rows: Vec<Vec<Option<String>>>,
    pub total: RowCount,
    pub matches: Option<RowCount>,
    pub elapsed_ms: u64,
    /// One `CellType` code per column for each row (`row_types`); `None` for Postgres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_types: Option<Vec<String>>,
    /// True when the page query has no tie-break: rows equal on the sort (all rows without a
    /// sort) have no fixed order, so paging can repeat or skip a row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unstable_order: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub latency_ms: u64,
    pub server_version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedInfo {
    pub profile: ConnectionProfile,
    pub server_version: String,
    pub schemas: Vec<String>,
    pub schema: String,
    /// True where a save runs one change at a time (a server without transactions).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub one_by_one_saves: bool,
}

/// Query parameter values by name. `None` binds NULL.
pub type ParamValues = BTreeMap<String, Option<String>>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuery {
    pub id: String,
    pub connection_id: String,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub last_params: ParamValues,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NewHistoryEntry {
    pub sql: String,
    pub params: ParamValues,
    pub verb: String,
    pub ok: bool,
    pub summary: String,
    /// Unix milliseconds.
    pub started_at: i64,
    pub elapsed_ms: u64,
    /// The database the query ran on when it wasn't the connection's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_database: Option<String>,
    /// The schema (a database, on MySQL and MongoDB) the query ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_schema: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    #[serde(flatten)]
    pub entry: NewHistoryEntry,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = r#"{"id":"c1","name":"Local","kind":"postgres","host":"localhost","port":5432,"user":"app","database":"app_dev","defaultSchema":null,"environment":"local","sslMode":"prefer","sslRootCert":null,"connectTimeoutSecs":10,"readOnly":false,"startupSql":null,"filePath":null}"#;

    fn network_target() -> NetworkTarget {
        NetworkTarget {
            host: "localhost".into(),
            port: 5432,
            user: "app".into(),
            database: "app_dev".into(),
            ssl_mode: SslMode::Prefer,
            ssl_root_cert: None,
            auth_source: None,
        }
    }

    const STORED_PG: &str = r#"{"id":"c1","name":"Local","kind":"postgres","environment":"local","defaultSchema":null,"connectTimeoutSecs":10,"readOnly":false,"startupSql":null,"transport":"network","host":"localhost","port":5432,"user":"app","database":"app_dev","sslMode":"prefer","sslRootCert":null}"#;
    const STORED_MYSQL: &str = r#"{"id":"c2","name":"My","kind":"mysql","environment":"staging","defaultSchema":"shop","connectTimeoutSecs":5,"readOnly":true,"startupSql":"SET x=1","transport":"network","host":"db","port":3306,"user":"root","database":"shop","sslMode":"require","sslRootCert":"/ca.pem"}"#;

    #[test]
    fn old_profile_json_without_auth_source_loads() {
        for stored in [STORED_PG, STORED_MYSQL] {
            let p: ConnectionProfile = serde_json::from_str(stored).unwrap();
            assert_eq!(p.network().unwrap().auth_source, None);
            assert_eq!(serde_json::to_string(&p).unwrap(), stored);
        }
        let mongo = STORED_PG.replace("postgres", "mongodb").replace(r#""sslRootCert":null"#, r#""sslRootCert":null,"authSource":"admin""#);
        let p: ConnectionProfile = serde_json::from_str(&mongo).unwrap();
        assert_eq!(p.kind, DbKind::Mongodb);
        assert_eq!(p.network().unwrap().auth_source.as_deref(), Some("admin"));
        assert_eq!(serde_json::to_string(&p).unwrap(), mongo);
    }

    #[test]
    fn profile_json_without_the_field_round_trips() {
        for stored in [LEGACY, STORED_PG, STORED_MYSQL] {
            let p: ConnectionProfile = serde_json::from_str(stored).unwrap();
            assert_eq!(p.statement_timeout_secs, None);
            assert!(!serde_json::to_string(&p).unwrap().contains("statementTimeoutSecs"));
        }
        for stored in [STORED_PG, STORED_MYSQL] {
            let p: ConnectionProfile = serde_json::from_str(stored).unwrap();
            assert_eq!(serde_json::to_string(&p).unwrap(), stored);
        }
        let mongo = STORED_PG.replace("postgres", "mongodb").replace(r#""startupSql":null"#, r#""startupSql":null,"statementTimeoutSecs":30"#);
        let p: ConnectionProfile = serde_json::from_str(&mongo).unwrap();
        assert_eq!(p.statement_timeout_secs, Some(30));
        assert_eq!(serde_json::to_string(&p).unwrap(), mongo);
    }

    #[test]
    fn mongodb_kind_serializes_as_mongodb() {
        assert_eq!(serde_json::to_string(&DbKind::Mongodb).unwrap(), "\"mongodb\"");
        assert_eq!(serde_json::from_str::<DbKind>("\"mongodb\"").unwrap(), DbKind::Mongodb);
    }

    #[test]
    fn cell_type_codes_round_trip_new() {
        let all = [
            (CellType::Null, 'n'),
            (CellType::Integer, 'i'),
            (CellType::Real, 'r'),
            (CellType::Text, 't'),
            (CellType::Blob, 'b'),
            (CellType::Long, 'l'),
            (CellType::Decimal, 'm'),
            (CellType::Bool, 'y'),
            (CellType::ObjectId, 'o'),
            (CellType::Date, 'd'),
            (CellType::Json, 'j'),
            (CellType::Other, 'x'),
        ];
        for (t, c) in all {
            assert_eq!(t.code(), c);
            assert_eq!(CellType::from_code(c), Some(t));
            assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{c}\""));
        }
        assert_eq!(CellType::from_code('z'), None);
    }

    #[test]
    fn connected_info_omits_one_by_one_saves_when_false() {
        let p: ConnectionProfile = serde_json::from_str(STORED_PG).unwrap();
        let mut info = ConnectedInfo { profile: p, server_version: "v".into(), schemas: vec![], schema: "s".into(), one_by_one_saves: false };
        assert!(serde_json::to_value(&info).unwrap().get("oneByOneSaves").is_none());
        info.one_by_one_saves = true;
        assert_eq!(serde_json::to_value(&info).unwrap()["oneByOneSaves"], true);
        let back: ConnectedInfo = serde_json::from_value(serde_json::to_value(&info).unwrap()).unwrap();
        assert!(back.one_by_one_saves);
    }

    #[test]
    fn legacy_profile_json_reads_as_network() {
        let p: ConnectionProfile = serde_json::from_str(LEGACY).unwrap();
        assert_eq!(p.transport, Transport::Network(network_target()));
        assert_eq!(p.id, "c1");
        assert_eq!(p.name, "Local");
        assert_eq!(p.kind, DbKind::Postgres);
        assert_eq!(p.environment, Environment::Local);
        assert_eq!(p.default_schema, None);
        assert_eq!(p.connect_timeout_secs, 10);
        assert!(!p.read_only);
        assert_eq!(p.startup_sql, None);
    }

    #[test]
    fn profile_round_trips_with_transport_tag() {
        let p: ConnectionProfile = serde_json::from_str(LEGACY).unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["transport"], "network");
        assert_eq!(v["host"], "localhost");
        assert!(v.get("filePath").is_none() && v.get("uri").is_none());
        let back: ConnectionProfile = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn file_profile_round_trips() {
        let json = r#"{"transport":"file","filePath":"/tmp/a.sqlite","id":"c2","name":"F","kind":"sqlite","environment":"local","connectTimeoutSecs":10}"#;
        let p: ConnectionProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.transport, Transport::File { file_path: "/tmp/a.sqlite".into() });
        assert_eq!(
            p.network().unwrap_err(),
            AppError::Connection("This connection has no server address.".into())
        );
        let back: ConnectionProfile = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn legacy_sqlite_profile_reads_as_file() {
        let json = LEGACY.replace(r#""kind":"postgres""#, r#""kind":"sqlite""#).replace(r#""filePath":null"#, r#""filePath":"/tmp/a.sqlite""#);
        let p: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p.transport, Transport::File { file_path: "/tmp/a.sqlite".into() });
        let null_path: ConnectionProfile =
            serde_json::from_str(&LEGACY.replace(r#""kind":"postgres""#, r#""kind":"sqlite""#)).unwrap();
        assert_eq!(null_path.transport, Transport::File { file_path: String::new() });
    }

    #[test]
    fn uri_profile_round_trips() {
        let json = r#"{"transport":"uri","uri":"mongodb://h/db","id":"c3","name":"U","kind":"postgres","environment":"local","connectTimeoutSecs":10}"#;
        let p: ConnectionProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.transport, Transport::Uri { uri: "mongodb://h/db".into() });
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["transport"], "uri");
        let back: ConnectionProfile = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn transport_tag_unknown_is_refused() {
        let json = r#"{"transport":"carrier","id":"c","name":"n","kind":"postgres","environment":"local","connectTimeoutSecs":10}"#;
        let err = serde_json::from_str::<ConnectionProfile>(json).unwrap_err().to_string();
        assert!(err.contains("unknown transport `carrier`"), "{err}");
    }

    #[test]
    fn function_info_without_kind_has_no_key() {
        let f = FunctionInfo { name: "f".into(), signature: "f(integer)".into(), kind: None };
        assert_eq!(serde_json::to_value(&f).unwrap(), serde_json::json!({"name": "f", "signature": "f(integer)"}));
        let p = FunctionInfo { kind: Some(RoutineKind::Procedure), ..f };
        assert_eq!(serde_json::to_value(&p).unwrap()["kind"], "procedure");
    }

    #[test]
    fn unknown_transport_tag_is_a_clear_error() {
        let json = r#"{"transport":"carrier","id":"c","name":"n","kind":"postgres","environment":"local","connectTimeoutSecs":10}"#;
        let err = serde_json::from_str::<ConnectionProfile>(json).unwrap_err().to_string();
        assert!(err.contains("unknown transport `carrier`"), "{err}");
    }

    #[test]
    fn network_of_a_uri_profile_is_an_error() {
        let json = r#"{"transport":"uri","uri":"mongodb://h/db","id":"c3","name":"U","kind":"postgres","environment":"local","connectTimeoutSecs":10}"#;
        let p: ConnectionProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.network().unwrap_err(), AppError::Connection("This connection has no server address.".into()));
    }

    #[test]
    fn untagged_network_profile_missing_fields_is_an_error() {
        let json = r#"{"id":"c","name":"n","kind":"postgres","environment":"local","connectTimeoutSecs":10}"#;
        assert!(serde_json::from_str::<ConnectionProfile>(json).is_err());
    }

    /// The JSON the UI sends (`src/api/types.ts`) must keep parsing into these variants.
    #[test]
    fn change_set_wire_contract() {
        let json = r#"{"table":{"schema":"fx","name":"users"},"changes":[{"kind":"update","key":["1"],"values":{"name":null}},{"kind":"insert","values":{}},{"kind":"delete","key":["2"]}]}"#;
        let set: ChangeSet = serde_json::from_str(json).unwrap();
        assert_eq!(
            set,
            ChangeSet {
                table: TableRef { schema: "fx".into(), name: "users".into() },
                changes: vec![
                    RowChange::Update {
                        key: vec!["1".into()],
                        values: [("name".to_string(), None)].into_iter().collect(),
                        original_types: BTreeMap::new(),
                    },
                    RowChange::Insert { values: CellValues::new() },
                    RowChange::Delete { key: vec!["2".into()], original_types: BTreeMap::new() },
                ],
            }
        );
    }

    /// Column names that are special in JavaScript objects are ordinary keys here
    /// (the change set `src/table/pending.test.ts` builds for such a page).
    #[test]
    fn change_set_with_prototype_named_columns_parses() {
        let json = r#"{"table":{"schema":"main","name":"t"},"changes":[{"kind":"update","key":["1"],"values":{"__proto__":"x","constructor":"2"},"originalTypes":{"id":"i","__proto__":"t","constructor":"r"}}]}"#;
        let set: ChangeSet = serde_json::from_str(json).unwrap();
        match &set.changes[0] {
            RowChange::Update { values, original_types, .. } => {
                assert_eq!(values["__proto__"].as_deref(), Some("x"));
                assert_eq!(original_types["__proto__"], CellType::Text);
                assert_eq!(original_types["constructor"], CellType::Real);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cell_type_codes_round_trip() {
        assert_eq!(serde_json::to_value(CellType::Blob).unwrap(), serde_json::json!("b"));
        assert_eq!(row_types(&[CellType::Integer, CellType::Text, CellType::Null, CellType::Blob]), "itnb");
        for t in [CellType::Null, CellType::Integer, CellType::Real, CellType::Text, CellType::Blob] {
            assert_eq!(CellType::from_code(t.code()), Some(t));
            assert_eq!(serde_json::from_value::<CellType>(serde_json::json!(t.code().to_string())).unwrap(), t);
        }
        assert_eq!(CellType::from_code('z'), None);
    }

    #[test]
    fn postgres_payloads_have_no_cell_types() {
        let page = TablePage {
            columns: vec![],
            primary_key: vec![],
            editing: Editing::Allowed,
            rows: vec![vec![Some("1".into())]],
            total: RowCount::Exact(1),
            matches: None,
            elapsed_ms: 3,
            cell_types: None,
            unstable_order: false,
        };
        let v = serde_json::to_value(&page).unwrap();
        assert!(v.get("cellTypes").is_none(), "{v}");
        let typed = serde_json::to_value(TablePage { cell_types: Some(vec!["i".into()]), ..page }).unwrap();
        assert_eq!(typed["cellTypes"], serde_json::json!(["i"]));

        let rows = crate::query::Outcome::Rows {
            columns: vec![],
            rows: vec![vec![Some("1".into())]],
            truncated: false,
            cell_types: None,
        };
        let v = serde_json::to_value(&rows).unwrap();
        assert!(v.get("cellTypes").is_none(), "{v}");
        let raw = crate::query::RawOutcome::Rows { columns: vec![], rows: vec![], truncated: false, cell_types: None };
        assert!(serde_json::to_value(&raw).unwrap().get("cellTypes").is_none());

        let json = r#"{"kind":"update","key":["1"],"values":{"name":"x"}}"#;
        let change: RowChange = serde_json::from_str(json).unwrap();
        match &change {
            RowChange::Update { original_types, .. } => assert!(original_types.is_empty()),
            other => panic!("{other:?}"),
        }
        assert_eq!(serde_json::to_string(&change).unwrap(), json);
        let del = r#"{"kind":"delete","key":["2"]}"#;
        assert_eq!(serde_json::to_string(&serde_json::from_str::<RowChange>(del).unwrap()).unwrap(), del);

        let typed = r#"{"kind":"delete","key":["2"],"originalTypes":{"id":"i"}}"#;
        let change: RowChange = serde_json::from_str(typed).unwrap();
        assert_eq!(
            change,
            RowChange::Delete { key: vec!["2".into()], original_types: [("id".to_string(), CellType::Integer)].into() }
        );
        assert_eq!(serde_json::to_string(&change).unwrap(), typed);
    }

    #[test]
    fn editing_wire_contract() {
        assert_eq!(serde_json::to_string(&Editing::NoPrimaryKey).unwrap(), r#"{"kind":"noPrimaryKey"}"#);
        assert_eq!(serde_json::to_string(&Editing::Allowed).unwrap(), r#"{"kind":"allowed"}"#);
    }

    #[test]
    fn page_order_flag_is_left_out_unless_set() {
        let page = TablePage {
            columns: vec![],
            primary_key: vec![],
            editing: Editing::NoPrimaryKey,
            rows: vec![],
            total: RowCount::Exact(0),
            matches: None,
            elapsed_ms: 1,
            cell_types: None,
            unstable_order: false,
        };
        let v = serde_json::to_value(&page).unwrap();
        assert!(v.get("unstableOrder").is_none(), "{v}");
        let back: TablePage = serde_json::from_value(v).unwrap();
        assert!(!back.unstable_order);
        let unstable = serde_json::to_value(TablePage { unstable_order: true, ..page }).unwrap();
        assert_eq!(unstable["unstableOrder"], serde_json::json!(true));
    }
}
