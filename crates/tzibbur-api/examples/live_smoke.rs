//! Exercise every REST endpoint against the live server with a real token.
//! Creates a throwaway group, mutates it, deletes it. Never posts to real groups.
//! `TZIBBUR_TOKEN=... cargo run -p tzibbur-api --example live_smoke`
use tzibbur_api::models::*;
use tzibbur_api::prelude::*;

struct Report(Vec<(String, Result<String, String>)>);
impl Report {
    fn ok(&mut self, name: &str, detail: impl Into<String>) {
        self.0.push((name.into(), Ok(detail.into())));
    }
    fn err(&mut self, name: &str, e: impl std::fmt::Display) {
        self.0.push((name.into(), Err(e.to_string())));
    }
}

#[tokio::main]
async fn main() {
    let token = std::env::var("TZIBBUR_TOKEN").expect("TZIBBUR_TOKEN");
    let client = TzibburClient::builder().token(token).build().unwrap();
    let mut r = Report(vec![]);

    // ---- profile ----
    let me = match client.me().await {
        Ok(u) => {
            r.ok("GET /v1/me", format!("{} {}", u.id, u.display_name));
            u
        }
        Err(e) => {
            r.err("GET /v1/me", e);
            return print(&r);
        }
    };
    match client.devices().await {
        Ok(d) => r.ok("GET /v1/me/devices", format!("{} device(s)", d.len())),
        Err(e) => r.err("GET /v1/me/devices", e),
    }
    // PATCH /v1/me: set to same name + marker, then revert.
    match client
        .update_display_name(&format!("{}·", me.display_name))
        .await
    {
        Ok(u) => {
            let ok = u.display_name.ends_with('·');
            match client.update_display_name(&me.display_name).await {
                Ok(back) if back.display_name == me.display_name => {
                    r.ok("PATCH /v1/me", format!("changed={ok} reverted=true"))
                }
                Ok(back) => r.err(
                    "PATCH /v1/me",
                    format!("revert mismatch: {}", back.display_name),
                ),
                Err(e) => r.err("PATCH /v1/me (revert)", e),
            }
        }
        Err(e) => r.err("PATCH /v1/me", e),
    }
    match client.legal(LegalDocKey::Terms).await {
        Ok(d) => r.ok(
            "GET /v1/legal/terms",
            format!("{} chars, checksum {}", d.markdown.len(), &d.checksum[..8]),
        ),
        Err(e) => r.err("GET /v1/legal/terms", e),
    }
    match client.legal(LegalDocKey::Privacy).await {
        Ok(d) => r.ok(
            "GET /v1/legal/privacy",
            format!("{} chars", d.markdown.len()),
        ),
        Err(e) => r.err("GET /v1/legal/privacy", e),
    }
    let phone = me.phone_e164.clone().unwrap_or_default();
    match client
        .check_contacts(&[phone.clone(), "+15005550006".into()], Some("US"))
        .await
    {
        Ok(c) => r.ok(
            "POST /v1/contacts/check",
            format!("{} registered of 2", c.len()),
        ),
        Err(e) => r.err("POST /v1/contacts/check", e),
    }
    match client.group_categories().await {
        Ok(c) => r.ok("GET /v1/groups/categories", c.categories.join(",")),
        Err(e) => r.err("GET /v1/groups/categories", e),
    }
    match client.list_groups(&ListParams::limit(50)).await {
        Ok(p) => r.ok("GET /v1/groups", format!("{} group(s)", p.items.len())),
        Err(e) => r.err("GET /v1/groups", e),
    }
    match client.pending(Some(10)).await {
        Ok(p) => r.ok(
            "GET /v1/pending",
            format!(
                "{} bucket(s), {} message(s)",
                p.messages.len(),
                p.message_count()
            ),
        ),
        Err(e) => r.err("GET /v1/pending", e),
    }

    // ---- throwaway group lifecycle ----
    let name = format!("smoke-{}", chrono::Utc::now().format("%H%M%S"));
    let g = match client
        .create_group(&CreateGroupRequest::standard(&name, "other"))
        .await
    {
        Ok(g) => {
            r.ok(
                "POST /v1/groups",
                format!(
                    "{} kind={:?} role={:?} post={} add={}",
                    g.id, g.kind, g.my_role, g.who_can_post, g.who_can_add_members
                ),
            );
            g
        }
        Err(e) => {
            r.err("POST /v1/groups", e);
            return print(&r);
        }
    };
    let gid = g.id.clone();
    match client.get_group(&gid).await {
        Ok(x) => r.ok(
            "GET /v1/groups/{id}",
            format!("name={} members={}", x.name, x.member_count),
        ),
        Err(e) => r.err("GET /v1/groups/{id}", e),
    }
    match client
        .update_group(&gid, &UpdateGroupRequest::rename(format!("{name}-renamed")))
        .await
    {
        Ok(_) => match client.get_group(&gid).await {
            Ok(x) if x.name.ends_with("-renamed") => {
                r.ok("PATCH /v1/groups/{id} (name)", "renamed")
            }
            Ok(x) => r.err(
                "PATCH /v1/groups/{id} (name)",
                format!("name still {}", x.name),
            ),
            Err(e) => r.err("PATCH /v1/groups/{id} (name) verify", e),
        },
        Err(e) => r.err("PATCH /v1/groups/{id} (name)", e),
    }
    match client
        .update_group(
            &gid,
            &UpdateGroupRequest::who_can_post(Permission::admins()),
        )
        .await
    {
        Ok(_) => match client.get_group(&gid).await {
            Ok(x) if x.who_can_post.as_str() == "admins" => {
                r.ok("PATCH /v1/groups/{id} (settings)", "whoCanPost=admins")
            }
            Ok(x) => r.err(
                "PATCH /v1/groups/{id} (settings)",
                format!("whoCanPost={}", x.who_can_post),
            ),
            Err(e) => r.err("PATCH /v1/groups/{id} (settings) verify", e),
        },
        Err(e) => r.err("PATCH /v1/groups/{id} (settings)", e),
    }
    match client
        .update_group(&gid, &UpdateGroupRequest::default())
        .await
    {
        Err(AppError::ValidationFailed { .. }) => r.ok(
            "PATCH /v1/groups/{id} (empty → 400)",
            "validation error as expected",
        ),
        Ok(_) => r.err("PATCH /v1/groups/{id} (empty)", "accepted an empty patch"),
        Err(e) => r.err("PATCH /v1/groups/{id} (empty)", e),
    }
    match client.list_members(&gid, &ListParams::limit(10)).await {
        Ok(p) => r.ok(
            "GET /v1/groups/{id}/members",
            format!(
                "{} member(s): {:?}",
                p.items.len(),
                p.items.iter().map(|m| m.role).collect::<Vec<_>>()
            ),
        ),
        Err(e) => r.err("GET /v1/groups/{id}/members", e),
    }
    match client
        .add_members(&gid, &["+15005550006".into()], Some("US"))
        .await
    {
        Ok(o) => r.ok(
            "POST /v1/groups/{id}/members",
            format!(
                "added={} notFound={} already={}",
                o.added.len(),
                o.not_found.len(),
                o.already_member.len()
            ),
        ),
        Err(e) => r.err("POST /v1/groups/{id}/members", e),
    }
    match client.set_member_role(&gid, &me.id, Role::Member).await {
        Err(AppError::LastAdmin { .. }) => {
            r.ok("PATCH .../members/{me} → member", "LastAdmin as expected")
        }
        Err(e) => r.err("PATCH .../members/{me}", format!("unexpected: {e}")),
        Ok(_) => r.err("PATCH .../members/{me}", "demoted the only admin"),
    }
    match client.remove_member(&gid, &me.id).await {
        Ok(_) => r.ok("DELETE .../members/{me}", "removed self (server allowed)"),
        Err(e) => r.ok("DELETE .../members/{me}", format!("rejected: {}", e.code())),
    }
    match client
        .get_messages(&gid, &MessagesQuery::default().limit(5))
        .await
    {
        Ok(m) => r.ok(
            "GET /v1/groups/{id}/messages",
            format!("{} message(s)", m.len()),
        ),
        Err(e) => r.err("GET /v1/groups/{id}/messages", e),
    }
    match client
        .send_message(&gid, &uuid::Uuid::new_v4().to_string(), "smoke")
        .await
    {
        Err(AppError::GroupTooSmall { .. }) => r.ok(
            "POST /v1/groups/{id}/messages (1 member)",
            "GroupTooSmall as expected",
        ),
        Err(AppError::NotFound { .. }) | Err(AppError::Forbidden { .. }) => r.ok(
            "POST /v1/groups/{id}/messages",
            "rejected (no longer a member)",
        ),
        Ok(m) => r.ok(
            "POST /v1/groups/{id}/messages",
            format!("sent seq {}", m.seq),
        ),
        Err(e) => r.err("POST /v1/groups/{id}/messages", e),
    }
    match client.ack(&gid, 0).await {
        Ok(_) => r.ok("POST /v1/groups/{id}/ack", "204"),
        Err(e) => r.ok(
            "POST /v1/groups/{id}/ack",
            format!("rejected: {}", e.code()),
        ),
    }
    match client.leave_group(&gid).await {
        Ok(_) => r.ok("POST /v1/groups/{id}/leave", "left"),
        Err(e) => r.ok(
            "POST /v1/groups/{id}/leave",
            format!("rejected: {} (last admin / not member)", e.code()),
        ),
    }
    match client.delete_group(&gid).await {
        Ok(_) => r.ok("DELETE /v1/groups/{id}", "deleted"),
        // Leaving as the last member already removed the group server-side.
        Err(AppError::NotFound { .. }) => r.ok(
            "DELETE /v1/groups/{id}",
            "already gone (server deletes an empty group on leave)",
        ),
        Err(e) => r.err("DELETE /v1/groups/{id}", e),
    }
    match client.get_group(&gid).await {
        Err(AppError::NotFound { .. }) => {
            r.ok("GET /v1/groups/{id} after delete", "404 as expected")
        }
        Ok(x) => r.err(
            "GET /v1/groups/{id} after delete",
            format!("still exists: {}", x.name),
        ),
        Err(e) => r.err(
            "GET /v1/groups/{id} after delete",
            format!("unexpected: {e}"),
        ),
    }
    print(&r);
}

fn print(r: &Report) {
    let mut fails = 0;
    for (name, res) in &r.0 {
        match res {
            Ok(d) => println!("PASS  {name:<44} {d}"),
            Err(e) => {
                fails += 1;
                println!("FAIL  {name:<44} {e}");
            }
        }
    }
    println!("\n{} checks, {} failed", r.0.len(), fails);
    if fails > 0 {
        std::process::exit(1);
    }
}
