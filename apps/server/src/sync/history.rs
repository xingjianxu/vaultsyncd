//! Authorized, bounded history/trash reads for local management and Web clients.
use super::*;
use vaultsyncd_domain::ids::EntryId;
use vaultsyncd_protocol::generated::{RecoveryItem, RecoveryPage};

type Row = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
fn cursor(query: Option<String>) -> Result<Vec<u8>, ()> {
    match query {
        None => Ok(vec![0; 16]),
        Some(q) => q
            .strip_prefix("after=")
            .and_then(parse_identity::<EntryId>)
            .map(|id| id.as_bytes().to_vec())
            .ok_or(()),
    }
}
/// Read one authorized page of immutable historical versions.
pub async fn list_history(
    State(context): State<Arc<SyncContext>>,
    UrlPath((vault, entry)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (caller, vault) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(entry) = parse_identity::<EntryId>(&entry) else {
        return bad_request(&caller.request_id, "invalid entry identity");
    };
    let after = match cursor(query) {
        Ok(v) => v,
        Err(()) => {
            return bad_request(
                &caller.request_id,
                "only one canonical after identity is accepted",
            );
        }
    };
    let result=async {
        let mut tx=context.auth.database().pool().begin().await?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM entries WHERE vault_id=? AND id=?)").bind(vault.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_one(&mut *tx).await?;
        if !exists {return Ok::<_,sqlx::Error>(None);}
        let rows:Vec<Row>=sqlx::query_as("SELECT lower(hex(v.id)),lower(hex(v.entry_id)),e.name,cast(e.revision AS TEXT),CASE WHEN e.parent_id IS NOT NULL THEN lower(hex(e.parent_id)) END,lower(hex(v.id)),lower(hex(v.object_hash)),cast(v.size AS TEXT),CASE WHEN e.deleted_at IS NOT NULL THEN cast(MAX(e.deleted_at+2592000,COALESCE(v.superseded_at+2592000,0)) AS TEXT) WHEN v.superseded_at IS NOT NULL THEN cast(v.superseded_at+2592000 AS TEXT) END,(SELECT lower(hex(c.source_entry_id)) FROM conflicts c WHERE c.vault_id=e.vault_id AND c.sibling_entry_id=e.id) FROM versions v JOIN entries e ON e.vault_id=v.vault_id AND e.id=v.entry_id WHERE v.vault_id=? AND v.entry_id=? AND v.id>? ORDER BY v.id LIMIT 101")
            .bind(vault.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).bind(after).fetch_all(&mut *tx).await?;
        tx.commit().await?;Ok(Some(page(rows)))
    }.await;
    match result {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => crate::uploads::not_found(&caller.request_id, vault),
        Err(_) => internal_error(
            &caller.request_id,
            "history unavailable",
            Some(vault.to_string()),
        ),
    }
}
/// Read one authorized page of retained recycle-bin entries.
pub async fn list_trash(
    State(context): State<Arc<SyncContext>>,
    UrlPath(vault): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (caller, vault) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let after = match cursor(query) {
        Ok(v) => v,
        Err(()) => {
            return bad_request(
                &caller.request_id,
                "only one canonical after identity is accepted",
            );
        }
    };
    let rows:Result<Vec<Row>,_>=sqlx::query_as("SELECT lower(hex(t.entry_id)),lower(hex(t.entry_id)),t.path,cast(e.revision AS TEXT),lower(hex(t.parent_id)),CASE WHEN t.content_version_id IS NOT NULL THEN lower(hex(t.content_version_id)) END,CASE WHEN v.object_hash IS NOT NULL THEN lower(hex(v.object_hash)) END,CASE WHEN v.size IS NOT NULL THEN cast(v.size AS TEXT) END,cast(t.expires_at AS TEXT),(SELECT lower(hex(c.source_entry_id)) FROM conflicts c WHERE c.vault_id=e.vault_id AND c.sibling_entry_id=e.id) FROM trash_entries t JOIN entries e ON e.vault_id=t.vault_id AND e.id=t.entry_id LEFT JOIN versions v ON v.vault_id=t.vault_id AND v.id=t.content_version_id WHERE t.vault_id=? AND t.entry_id>? ORDER BY t.entry_id LIMIT 101")
        .bind(vault.as_bytes().as_slice()).bind(after).fetch_all(context.auth.database().pool()).await;
    match rows {
        Ok(rows) => Json(page(rows)).into_response(),
        Err(_) => internal_error(
            &caller.request_id,
            "trash unavailable",
            Some(vault.to_string()),
        ),
    }
}
fn page(mut rows: Vec<Row>) -> RecoveryPage {
    let more = rows.len() > 100;
    rows.truncate(100);
    let next_cursor = if more {
        rows.last().map(|r| r.0.clone())
    } else {
        None
    };
    RecoveryPage {
        items: rows
            .into_iter()
            .map(
                |(
                    id,
                    entry_id,
                    name,
                    revision,
                    parent_id,
                    version_id,
                    hash,
                    size,
                    expires_at,
                    conflict_source,
                )| RecoveryItem {
                    id,
                    entry_id,
                    name,
                    revision,
                    parent_id,
                    version_id,
                    hash,
                    size,
                    expires_at,
                    conflict_source,
                },
            )
            .collect(),
        next_cursor,
    }
}
/// Current authorized entries for recovery selection; full relative path is computed from identities.
pub async fn list_recovery(
    State(context): State<Arc<SyncContext>>,
    UrlPath(vault): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    list_current(context, vault, query, headers, false).await
}
/// Conflict copies are identified by durable mapping, never inferred from filename.
pub async fn list_conflicts(
    State(context): State<Arc<SyncContext>>,
    UrlPath(vault): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    list_current(context, vault, query, headers, true).await
}
async fn list_current(
    context: Arc<SyncContext>,
    vault: String,
    query: Option<String>,
    headers: HeaderMap,
    conflicts_only: bool,
) -> Response {
    let (caller, vault) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let after = match cursor(query) {
        Ok(v) => v,
        Err(()) => return bad_request(&caller.request_id, "invalid after identity"),
    };
    let rows: Result<Vec<Row>,_> = sqlx::query_as("WITH RECURSIVE paths(id,path) AS (SELECT id,'' FROM entries WHERE vault_id=? AND parent_id IS NULL AND deleted_at IS NULL UNION ALL SELECT e.id,CASE WHEN p.path='' THEN e.name ELSE p.path||'/'||e.name END FROM entries e JOIN paths p ON e.parent_id=p.id WHERE e.vault_id=? AND e.deleted_at IS NULL) SELECT lower(hex(e.id)),lower(hex(e.id)),p.path,cast(e.revision AS TEXT),CASE WHEN e.parent_id IS NOT NULL THEN lower(hex(e.parent_id)) END,CASE WHEN v.id IS NOT NULL THEN lower(hex(v.id)) END,CASE WHEN v.object_hash IS NOT NULL THEN lower(hex(v.object_hash)) END,CASE WHEN v.size IS NOT NULL THEN cast(v.size AS TEXT) END,NULL,CASE WHEN c.source_entry_id IS NOT NULL THEN lower(hex(c.source_entry_id)) END FROM paths p JOIN entries e ON e.id=p.id LEFT JOIN versions v ON v.vault_id=e.vault_id AND v.entry_id=e.id AND v.superseded_at IS NULL LEFT JOIN conflicts c ON c.vault_id=e.vault_id AND c.sibling_entry_id=e.id WHERE e.vault_id=? AND e.parent_id IS NOT NULL AND e.id>? AND (?=0 OR c.source_entry_id IS NOT NULL) ORDER BY e.id LIMIT 101")
    .bind(vault.as_bytes().as_slice()).bind(vault.as_bytes().as_slice()).bind(vault.as_bytes().as_slice()).bind(after).bind(conflicts_only).fetch_all(context.auth.database().pool()).await;
    match rows {
        Ok(rows) => Json(page(rows)).into_response(),
        Err(_) => internal_error(
            &caller.request_id,
            "recovery entries unavailable",
            Some(vault.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_page_has_an_exact_resume_identity_without_losing_the_tail() {
        let rows: Vec<Row> = (1..=101)
            .map(|i| {
                (
                    format!("{i:032x}"),
                    "11".repeat(16),
                    "file.txt".into(),
                    "9007199254740993".into(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            })
            .collect();
        let page = page(rows);
        assert_eq!(page.items.len(), 100);
        assert_eq!(page.next_cursor, Some(format!("{:032x}", 100)));
        assert_eq!(page.items[99].revision, "9007199254740993");
        assert!(cursor(Some(format!("after={}", page.next_cursor.unwrap()))).is_ok());
        for invalid in [
            "after=abc",
            "after=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "after=11111111111111111111111111111111&limit=1",
            "ignored=1",
        ] {
            assert!(cursor(Some(invalid.into())).is_err());
        }
        assert!(super::page(vec![]).next_cursor.is_none());
    }
}
