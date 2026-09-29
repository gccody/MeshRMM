use crate::*;

pub(crate) async fn account(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    account_for_identity(environment, identity).await
}

async fn account_for_identity(environment: &Env, identity: Identity) -> Result<Response> {
    let db = environment.d1("DB")?;
    let company = query!(
        &db,
        "SELECT id, name, dashboard_idle_timeout_minutes, blackout_message, display_border, prevent_idle_lock, allow_idle_override, idle_disconnect_minutes, allow_idle_disconnect_override, clear_clipboard_on_close, allow_clear_clipboard_override, session_banner, connection_notification, background_connection_notification, connection_notification_message, connection_approval, connection_approval_message, connection_approval_timeout_seconds, connection_approval_lock_idle_seconds, slug, status FROM companies WHERE id = ?1",
        identity.company_id
    )?
    .metered_first::<Company>(None)
    .await?;
    Response::from_json(&AccountResponse {
        user_id: identity.user_id,
        company,
        role: identity.role,
        roles: identity.roles,
        permissions: identity.permissions,
    })
}

pub(crate) async fn update_company_settings(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_company_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: UpdateCompanySettingsRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return api_error(400, "invalid company settings request"),
    };
    let timeout = match validate_dashboard_idle_timeout(body.dashboard_idle_timeout_minutes) {
        Ok(timeout) => timeout,
        Err(_) => {
            return api_error(
                400,
                "dashboard idle timeout must be between 5 and 1440 minutes",
            );
        }
    };
    if body
        .idle_disconnect_minutes
        .is_some_and(|minutes| !meshrmm_protocol_types::valid_idle_disconnect_minutes(minutes))
    {
        return api_error(
            400,
            "idle disconnect time must be never or one of the offered choices",
        );
    }
    if body
        .blackout_message
        .as_deref()
        .is_some_and(|text| !meshrmm_protocol_types::valid_blackout_message(text))
    {
        return api_error(
            400,
            "blackout message must be nonempty, at most 2048 UTF-8 bytes, and contain no control characters except newlines",
        );
    }
    if body
        .connection_notification_message
        .as_deref()
        .is_some_and(|text| !meshrmm_protocol_types::valid_connection_notification_message(text))
    {
        return api_error(
            400,
            "connection notification message must be nonempty, at most 512 UTF-8 bytes, and contain no control characters except newlines",
        );
    }
    if body
        .connection_approval_message
        .as_deref()
        .is_some_and(|text| !meshrmm_protocol_types::valid_connection_approval_message(text))
    {
        return api_error(
            400,
            "connection approval message must be nonempty, at most 512 UTF-8 bytes, and contain no control characters except newlines",
        );
    }
    if body
        .connection_approval_timeout_seconds
        .is_some_and(|seconds| !meshrmm_protocol_types::valid_connection_approval_timeout(seconds))
    {
        return api_error(
            400,
            "connection approval timeout must be between 5 and 300 seconds",
        );
    }
    if body
        .connection_approval_lock_idle_seconds
        .is_some_and(|seconds| {
            !meshrmm_protocol_types::valid_connection_approval_lock_idle(seconds)
        })
    {
        return api_error(
            400,
            "connection approval lock screen idle time must be between 0 and 3600 seconds",
        );
    }
    let db = environment.d1("DB")?;
    let company_exists = query!(
        &db,
        "SELECT id, name, dashboard_idle_timeout_minutes, blackout_message, display_border, prevent_idle_lock, allow_idle_override, idle_disconnect_minutes, allow_idle_disconnect_override, clear_clipboard_on_close, allow_clear_clipboard_override, session_banner, connection_notification, background_connection_notification, connection_notification_message, connection_approval, connection_approval_message, connection_approval_timeout_seconds, connection_approval_lock_idle_seconds, slug, status FROM companies WHERE id = ?1",
        identity.company_id
    )?
    .metered_first::<Company>(None)
    .await?
    .is_some();
    if !company_exists {
        return api_error(404, "company has not been provisioned");
    }
    query!(
        &db,
        "UPDATE companies SET dashboard_idle_timeout_minutes = ?1, blackout_message = COALESCE(?2, blackout_message), display_border = COALESCE(?4, display_border), prevent_idle_lock = COALESCE(?5, prevent_idle_lock), allow_idle_override = COALESCE(?6, allow_idle_override), session_banner = COALESCE(?7, session_banner), connection_notification = COALESCE(?8, connection_notification), connection_notification_message = COALESCE(?9, connection_notification_message), background_connection_notification = COALESCE(?10, background_connection_notification), idle_disconnect_minutes = ?11, allow_idle_disconnect_override = COALESCE(?12, allow_idle_disconnect_override), clear_clipboard_on_close = COALESCE(?13, clear_clipboard_on_close), allow_clear_clipboard_override = COALESCE(?14, allow_clear_clipboard_override), connection_approval = COALESCE(?15, connection_approval), connection_approval_message = COALESCE(?16, connection_approval_message), connection_approval_timeout_seconds = COALESCE(?17, connection_approval_timeout_seconds), connection_approval_lock_idle_seconds = COALESCE(?18, connection_approval_lock_idle_seconds) WHERE id = ?3",
        timeout,
        body.blackout_message.clone(),
        identity.company_id,
        body.display_border.map(i32::from),
        body.prevent_idle_lock.map(i32::from),
        body.allow_idle_override.map(i32::from),
        body.session_banner.map(i32::from),
        body.connection_notification.map(i32::from),
        body.connection_notification_message.clone(),
        body.background_connection_notification.map(i32::from),
        body.idle_disconnect_minutes,
        body.allow_idle_disconnect_override.map(i32::from),
        body.clear_clipboard_on_close.map(i32::from),
        body.allow_clear_clipboard_override.map(i32::from),
        body.connection_approval.map(i32::from),
        body.connection_approval_message.clone(),
        body.connection_approval_timeout_seconds,
        body.connection_approval_lock_idle_seconds
    )?
    .metered_run()
    .await?;
    audit(
        &db,
        &identity,
        "company.settings.update",
        "company",
        &identity.company_id,
        &serde_json::json!({ "dashboard_idle_timeout_minutes": timeout, "blackout_message": body.blackout_message, "display_border": body.display_border, "prevent_idle_lock": body.prevent_idle_lock, "allow_idle_override": body.allow_idle_override, "idle_disconnect_minutes": body.idle_disconnect_minutes, "allow_idle_disconnect_override": body.allow_idle_disconnect_override, "clear_clipboard_on_close": body.clear_clipboard_on_close, "allow_clear_clipboard_override": body.allow_clear_clipboard_override, "session_banner": body.session_banner, "connection_notification": body.connection_notification, "background_connection_notification": body.background_connection_notification, "connection_notification_message": body.connection_notification_message, "connection_approval": body.connection_approval, "connection_approval_message": body.connection_approval_message, "connection_approval_timeout_seconds": body.connection_approval_timeout_seconds, "connection_approval_lock_idle_seconds": body.connection_approval_lock_idle_seconds }).to_string(),
    )
    .await?;
    account_for_identity(environment, identity).await
}
