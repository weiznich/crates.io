use crate::models::ApiToken;
use crate::schema::api_tokens;
use crate::{email::Email, models::User, worker::Environment, Emails};
use anyhow::anyhow;
use chrono::SecondsFormat;
use crates_io_worker::BackgroundJob;
use diesel::dsl::now;
use diesel::prelude::*;
use std::sync::Arc;

/// The threshold for the expiry notification.
const EXPIRY_THRESHOLD: chrono::TimeDelta = chrono::TimeDelta::days(3);

/// The maximum number of tokens to check per run.
const MAX_ROWS: i64 = 10000;

#[derive(Default, Serialize, Deserialize, Debug)]
pub struct SendTokenExpiryNotifications;

impl BackgroundJob for SendTokenExpiryNotifications {
    const JOB_NAME: &'static str = "send_token_expiry_notifications";

    type Context = Arc<Environment>;

    #[instrument(skip(env), err)]
    async fn run(&self, env: Self::Context) -> anyhow::Result<()> {
        let conn = env.deadpool.get().await?;
        conn.interact(move |conn| {
            // Check if the token is about to expire
            // If the token is about to expire, trigger a notification.
            check(&env.emails, conn)
        })
        .await
        .map_err(|err| anyhow!(err.to_string()))?
    }
}

/// Find tokens that are about to expire and send notifications to their owners.
fn check(emails: &Emails, conn: &mut PgConnection) -> anyhow::Result<()> {
    let before = chrono::Utc::now() + EXPIRY_THRESHOLD;
    info!("Searching for tokens that will expire before {before}…");

    let expired_tokens = find_expiring_tokens(conn, before)?;
    let num_tokens = expired_tokens.len();
    if num_tokens == 0 {
        info!("Found no tokens that will expire before {before}. Skipping expiry notifications.");
        return Ok(());
    }

    info!("Found {num_tokens} tokens that will expire before {before}. Sending out expiry notifications…");

    if num_tokens == MAX_ROWS as usize {
        warn!("The maximum number of API tokens per query has been reached. More API tokens might be processed on the next run.");
    }

    let mut success = 0;
    for token in &expired_tokens {
        if let Err(e) = handle_expiring_token(conn, token, emails) {
            error!(?e, "Failed to handle expiring token");
        } else {
            success += 1;
        }
    }

    info!("Sent expiry notifications for {success} of {num_tokens} expiring tokens.");

    Ok(())
}

/// Send an email to the user associated with the token.
fn handle_expiring_token(
    conn: &mut PgConnection,
    token: &ApiToken,
    emails: &Emails,
) -> Result<(), anyhow::Error> {
    debug!("Looking up user {} for token {}…", token.user_id, token.id);
    let user = User::find(conn, token.user_id)?;

    debug!("Looking up email address for user {}…", user.id);
    let recipient = user
        .email(conn)?
        .ok_or_else(|| anyhow!("No address found"))?;

    debug!("Sending expiry notification to {}…", recipient);
    let email = ExpiryNotificationEmail {
        name: &user.gh_login,
        token_name: &token.name,
        expiry_date: token.expired_at.unwrap().and_utc(),
    };
    emails.send(&recipient, email)?;

    // Update the token to prevent duplicate notifications.
    debug!("Marking token {} as notified…", token.id);
    diesel::update(token)
        .set(api_tokens::expiry_notification_at.eq(now.nullable()))
        .execute(conn)?;

    Ok(())
}

/// Find tokens that will expire before the given date, but haven't expired yet
/// and haven't been notified about their impending expiry. Revoked tokens are
/// also ignored.
///
/// This function returns at most `MAX_ROWS` tokens.
pub fn find_expiring_tokens(
    conn: &mut PgConnection,
    before: chrono::DateTime<chrono::Utc>,
) -> QueryResult<Vec<ApiToken>> {
    api_tokens::table
        .filter(api_tokens::revoked.eq(false))
        .filter(api_tokens::expired_at.is_not_null())
        // Ignore already expired tokens
        .filter(api_tokens::expired_at.assume_not_null().gt(now))
        .filter(
            api_tokens::expired_at
                .assume_not_null()
                .lt(before.naive_utc()),
        )
        .filter(api_tokens::expiry_notification_at.is_null())
        .select(ApiToken::as_select())
        .order_by(api_tokens::expired_at.asc()) // The most urgent tokens first
        .limit(MAX_ROWS)
        .get_results(conn)
}

#[derive(Debug, Clone)]
struct ExpiryNotificationEmail<'a> {
    name: &'a str,
    token_name: &'a str,
    expiry_date: chrono::DateTime<chrono::Utc>,
}

impl<'a> Email for ExpiryNotificationEmail<'a> {
    const SUBJECT: &'static str = "Your token is about to expire";

    fn body(&self) -> String {
        format!(
            r#"Hi {},

We noticed your token "{}" will expire on {}.

If this token is still needed, visit https://crates.io/settings/tokens/new to generate a new one.

Thanks,
The crates.io team"#,
            self.name,
            self.token_name,
            self.expiry_date.to_rfc3339_opts(SecondsFormat::Secs, true)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewUser;
    use crate::{
        models::token::ApiToken, schema::api_tokens, test_util::test_db_connection,
        util::token::PlainToken,
    };
    use diesel::dsl::IntervalDsl;
    use lettre::Address;

    #[tokio::test]
    async fn test_expiry_notification() -> anyhow::Result<()> {
        let emails = Emails::new_in_memory();
        let (_test_db, mut conn) = test_db_connection();

        // Set up a user and a token that is about to expire.
        let user = NewUser::new(0, "a", None, None, "token").create_or_update(
            Some("testuser@test.com"),
            &Emails::new_in_memory(),
            &mut conn,
        )?;
        let token = PlainToken::generate();

        let token: ApiToken = diesel::insert_into(api_tokens::table)
            .values((
                api_tokens::user_id.eq(user.id),
                api_tokens::name.eq("test_token"),
                api_tokens::token.eq(token.hashed()),
                api_tokens::expired_at.eq(now.nullable() + (EXPIRY_THRESHOLD.num_days() - 1).day()),
            ))
            .returning(ApiToken::as_returning())
            .get_result(&mut conn)?;

        // Insert a few tokens that are not set to expire.
        let not_expired_offset = EXPIRY_THRESHOLD.num_days() + 1;
        for i in 0..3 {
            let token = PlainToken::generate();
            diesel::insert_into(api_tokens::table)
                .values((
                    api_tokens::user_id.eq(user.id),
                    api_tokens::name.eq(format!("test_token{i}")),
                    api_tokens::token.eq(token.hashed()),
                    api_tokens::expired_at.eq(now.nullable() + not_expired_offset.day()),
                ))
                .returning(ApiToken::as_returning())
                .get_result(&mut conn)?;
        }

        // Check that the token is about to expire.
        check(&emails, &mut conn)?;

        // Check that an email was sent.
        let sent_mail = emails.mails_in_memory().unwrap();
        assert_eq!(sent_mail.len(), 1);
        let sent = &sent_mail[0];
        assert_eq!(&sent.0.to(), &["testuser@test.com".parse::<Address>()?]);
        assert!(sent.1.contains("Your token is about to expire"));
        let updated_token = api_tokens::table
            .filter(api_tokens::id.eq(token.id))
            .filter(api_tokens::expiry_notification_at.is_not_null())
            .select(ApiToken::as_select())
            .first::<ApiToken>(&mut conn)?;
        assert_eq!(updated_token.name, "test_token".to_owned());

        // Check that the token is not about to expire.
        let tokens = api_tokens::table
            .filter(api_tokens::revoked.eq(false))
            .filter(api_tokens::expiry_notification_at.is_null())
            .select(ApiToken::as_select())
            .load::<ApiToken>(&mut conn)?;
        assert_eq!(tokens.len(), 3);

        // Insert a already expired token.
        let token = PlainToken::generate();
        diesel::insert_into(api_tokens::table)
            .values((
                api_tokens::user_id.eq(user.id),
                api_tokens::name.eq("expired_token"),
                api_tokens::token.eq(token.hashed()),
                api_tokens::expired_at.eq(now.nullable() - 1.day()),
            ))
            .returning(ApiToken::as_returning())
            .get_result(&mut conn)?;

        // Check that the token is not about to expire.
        check(&emails, &mut conn)?;

        // Check that no email was sent.
        let sent_mail = emails.mails_in_memory().unwrap();
        assert_eq!(sent_mail.len(), 1);

        Ok(())
    }
}
