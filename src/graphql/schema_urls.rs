pub(crate) fn graphql_concept_kind(kind: &str) -> &'static str {
    match kind {
        "OBJECT" => "graphql_type",
        "INPUT_OBJECT" => "graphql_input_object",
        "INTERFACE" => "graphql_interface",
        "UNION" => "graphql_union",
        "ENUM" => "graphql_enum",
        "SCALAR" => "graphql_scalar",
        _ => "graphql_type",
    }
}

pub(crate) fn graphql_reference_path(version: &str, kind: &str, name: &str) -> Option<String> {
    let section = match kind {
        "OBJECT" => "objects",
        "INPUT_OBJECT" => "input-objects",
        "INTERFACE" => "interfaces",
        "UNION" => "unions",
        "ENUM" => "enums",
        "SCALAR" => "scalars",
        _ => return None,
    };
    Some(format!(
        "/docs/api/admin-graphql/{version}/{section}/{name}"
    ))
}

pub(crate) fn concept_id(version: &str, name: &str) -> String {
    format!("admin_graphql.{version}.{name}")
}

pub(crate) fn admin_graphql_direct_proxy_url(version: &str) -> String {
    format!("https://shopify.dev/admin-graphql-direct-proxy/{version}")
}
