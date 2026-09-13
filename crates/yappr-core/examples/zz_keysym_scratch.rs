//! TEMPORARY diagnostic. Delete after use.
//!
//! Round 3. Real use showed a *re-opened* session taking 3,4 s where the
//! startup one took 12 ms, which latched `KEEP_OPEN` and left the
//! screen-sharing indicator up. This asks why: does closing a session cost
//! the restore token, and is the slow re-open a dialog or just slow?
//!
//! Opens, closes and re-opens four times with growing gaps, printing each `Start` duration
//! and whether a token came back. No keys are pressed and no clipboard is
//! touched.
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;

fn main() {
    futures_lite::future::block_on(run());
}

async fn once(proxy: &RemoteDesktop, label: &str, token: Option<String>) -> Option<String> {
    let session = match proxy.create_session(Default::default()).await {
        Ok(s) => s,
        Err(e) => {
            println!("{label}: create_session failed: {e}");
            return token;
        }
    };
    if let Err(e) = proxy
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(BitFlags::from(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await
    {
        println!("{label}: select_devices failed: {e}");
        return token;
    }
    let started = std::time::Instant::now();
    let response = match proxy.start(&session, None, Default::default()).await {
        Ok(r) => match r.response() {
            Ok(r) => r,
            Err(e) => {
                println!("{label}: Start refused: {e}");
                return token;
            }
        },
        Err(e) => {
            println!("{label}: Start failed: {e}");
            return token;
        }
    };
    let elapsed = started.elapsed();
    let new_token = response.restore_token().map(str::to_owned);
    println!(
        "{label}: Start {:>8.1?}  token_offered={}  token_issued={}  same_token={}",
        elapsed,
        token.is_some(),
        new_token.is_some(),
        match (&token, &new_token) {
            (Some(a), Some(b)) => (a == b).to_string(),
            _ => "-".to_string(),
        }
    );
    // Prove the session is live before closing it: one modifier press and
    // release, which types nothing into anything.
    let _ = proxy
        .notify_keyboard_keysym(&session, 0xffe1, KeyState::Pressed, Default::default())
        .await;
    let _ = proxy
        .notify_keyboard_keysym(&session, 0xffe1, KeyState::Released, Default::default())
        .await;
    let closed = std::time::Instant::now();
    match session.close().await {
        Ok(()) => println!("{label}: closed in {:?}", closed.elapsed()),
        Err(e) => println!("{label}: close failed: {e}"),
    }
    new_token.or(token)
}

async fn run() {
    let token_path = yappr_core::libei::token_file();
    let mut token = std::fs::read_to_string(&token_path).ok().map(|s| s.trim().to_string());
    println!("starting token present: {}", token.is_some());

    let proxy = RemoteDesktop::new().await.expect("RemoteDesktop portal");

    for (i, gap) in [5u64, 45, 90, 0].into_iter().enumerate() {
        token = once(&proxy, &format!("open #{}", i + 1), token).await;
        if gap > 0 {
            println!("  ... waiting {gap}s");
            std::thread::sleep(Duration::from_secs(gap));
        }
    }

    if let Some(t) = &token {
        let _ = std::fs::write(&token_path, t);
        println!("wrote the latest token back to {}", token_path.display());
    }
}
