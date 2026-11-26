# Mobile notifications

Your phone can alert you when an agent finishes, fails, waits for an approval,
asks a question or stops at a limit. Tap the alert to open that thread. Android
and iOS carry the same record and alert on the same transitions. Android can
additionally keep a running turn on screen as an ongoing **Live Update**; iOS
cannot without an ActivityKit widget extension, which this repository does not
build. Their lock-screen behaviour differs where iOS cannot run app code; the
limits below say exactly where. Delivery goes through
Connect only: a direct or Tailscale pairing is not enough, because the daemon
does not hold a phone's push token and a push provider cannot reach a LAN
address.

## Turning alerts on

1. Sign in to Connect in h4rn and link the host that runs the agents.
2. On that host, open **Settings → Notifications** and turn on **Send agent
   alerts to my phone through Connect**. This is a machine setting: it is off
   until you turn it on, and turning it off stops the next publish. Turning it
   off works from any client, even one that is not signed in to Connect, because
   the setting belongs to the machine that publishes.
3. In the h4rn phone app, open **Settings → Notifications → Phone alerts** and
   turn on **Device notifications**. The phone asks for notification permission
   the first time (Android 13 and newer; on iOS the system dialog appears when
   you turn the switch on). The row shows the real state: the OS permission,
   whether this phone registered with Connect (not whether another phone on the
   account did), and whether the Connect server has a push sender for this
   phone's platform. A status call that fails is reported as unknown, not as a
   missing sender.
4. On Android you can also turn on **Show running agents as live
   notifications**. It is a separate switch, off by default, for an ongoing
   notification that names a running thread, its project and what the agent is
   doing (Working, Waiting for approval, Asked a question). It still needs
   **Device notifications** on and the phone signed in. See [While an agent is
   running](#while-an-agent-is-running-android-live-updates) below. iOS is not
   offered this switch.

Alerts are quiet only when the app is in the foreground **and** that exact
thread is on screen. Every other thread still alerts, as does the same thread
while the app is in the background. A grouped alert names no single thread, so
it is never quiet for the thread you happen to have open: the other agents in
the group still need you. Tapping a grouped alert opens h4rn instead of one
thread.

Turning **Device notifications** off removes this phone's registration from
Connect. If that request cannot reach Connect (the phone is offline, or the
relay errors), the app remembers the removal and retries it the next time it is
signed in and opened, so the row does not linger. On Android the phone also
stays silent meanwhile: its notification service ignores data messages while
the switch is off. iOS clears everything already in Notification Centre and
keeps a foreground alert quiet, but for a suspended app that is not a
lock-screen guarantee; see the iOS limits below.

Signing out does two things. The phone's local "signed in and switch on" flag
goes off at once, whatever the relay can be reached for. On Android that is
what keeps the signed-out phone from showing the old account's thread titles on
the lock screen, because its notification service checks the flag before
showing a data message. A pending removal is kept as well and retried the next
time this phone signs in, because the relay row belongs to the account that
signed out. After the credentials are gone the app cannot retry on its own, and
signing out through `POST /v1/auth/logout` also deletes the token-scoped row on
the relay. Turning the switch on again after signing in re-registers the phone.
On iOS the system, not the app, shows a suspended app's alert, so a signed-out
iPhone that cannot reach Connect may still show alerts until the pending
removal succeeds at the next sign-in. That case is listed under iOS limits.

## What leaves the daemon

Each alert carries exactly this record:

| Field | Example | Notes |
|---|---|---|
| thread id | `th_0192f0…` | Opens the thread on tap |
| thread title | `Fix the flaky login test` | As shown in the sidebar |
| project name | `h4rn` | Folder name, not the full path |
| state | `working`, `completed`, `failed`, `approval`, `input`, `limit` | `working` is Android's ongoing update; the rest alert |
| transition id | `0192f0a1-…` | Identity of this transition; only used to tell a replay from a new event |
| timestamp | RFC 3339 | When the daemon observed it |

There is no field for a prompt, a reply, reasoning, tool output, a diff, a file
path, a terminal byte or a credential. The record is sent over the daemon's
existing relay connection and is separate from the Noise-encrypted session.

A finished turn is classified by its own status, not by the agent state it lands
in. A turn you stopped yourself (`Interrupted`) does not alert. A usage, token or
turn limit that auto-resume will not continue asks you to come back with the
`limit` state; a limit that auto-resume is already handling stays quiet.

Connect stores the last transition delivered per (user, thread), keyed by the
transition id. A replay of one frame never alerts twice, while a later
completion or approval that repeats the same state does. Connect also drops a
completion that is more than two minutes old. Transitions that arrive together
are grouped: two or more agents waiting for a person become one **N agents need
attention** alert, and two or more finishing become one **N agents finished**
alert. The count is of threads, not raw transitions, so one agent that raises the
same hand twice inside the group window is still one agent. When a turn start
sends a collecting group early (see below), the next group gets its own full
window: the earlier group's timer never sends it early, so a burst of finishes
that follows is not split into one alert per thread.

## While an agent is running (Android Live Updates)

With **Show running agents as live notifications** on, an Android phone keeps
one ongoing notification for each running thread. It shows the thread title, the
project name and a state word: **Working**, **Waiting for approval** or **Asked
a question**. Tapping it opens the thread. Android 16 promotes it to the status
bar's Live Update surface with an indeterminate progress style when it can: the
app declares the `POST_PROMOTED_NOTIFICATIONS` permission, and the person can
still turn promotion off for h4rn in the system's notification settings. Without
promotion, and on older releases, the same ongoing notification shows in the
shade.

The switch needs **Device notifications** on and allowed; until then it is
shown but cannot be changed, with a note saying so. Live Updates also need the
phone app at version 1.0.2 or later: Connect never sends the `working` state to
an Android registration from an older (or unversioned) build, because that build
would show "Working" as an ordinary alert on every turn start.

**The next mobile release must be 1.0.2 or later for Live Updates to arrive.**
The phone app in this repository is still versioned 1.0.1, so a build made from
it today registers as 1.0.1 and is never sent `working`: the switch is there,
but no running turn appears until a release at 1.0.2 or later is installed.

The daemon publishes `working` once when a turn starts, never per token, per
tool call or per summary update. The next state for that thread updates the same
notification: an approval or a question changes the state word and still gets
the person's attention, while a turn that starts is silent. Once the last open
approval or question is answered and the turn keeps running, the notification
goes back to **Working**. A finish, failure or
limit replaces the ongoing notification with the normal alert, because both use
the thread's own notification id. When a queued message starts the next turn
straight away, Connect sends the previous turn's finish before the new turn's
`working` frame, so the finish never replaces the running turn's entry. The
reverse holds too: a finish that reaches Connect after a turn start is always
sent after it, so the phone never shows **Working** for a turn that has ended.
Connect sends from a queue per account, so a slow push provider delays only that
account's notifications, never Connect reading the machine's connection, and
one delivery that fails outright does not stop the ones queued behind it.

A turn stopped by the person has no alert, and a grouped finish names no
thread. Every time the app receives the daemon's thread list, the first one
after a cold start included, it tells the phone which of that machine's threads
are not running, and the phone removes their ongoing notifications. A thread
the list does not name is left alone, since it may be running on another of
your machines. So an entry left by a Stop, a grouped finish, or a turn that
ended while the app was closed goes away as soon as the app is open and
connected to that thread's machine. Opening a thread,
or returning to the app while that thread is on screen, removes its entry too.

The ongoing notification follows the same rules as an alert: none while the
switch is off or the phone is signed out, and none for the thread on screen in
the foreground. With the switch off, a turn start leaves the tray alone unless
the phone still shows a Live Update for that thread from before: the thread's
last Finished, Failed or limit alert stays until you read it. A grouped alert names no thread, so it is never an ongoing
update; each thread's own entry stays until its turn ends. Turning the switch
off, turning **Device notifications** off or signing out removes what is already
showing.

## How each phone is told

Connect records the platform on each phone's registration row and sends that
phone the shape its OS needs. Both shapes are built from the same record and are
sent to one phone at a time.

**Android** receives an FCM data message under `data`: `h4rn_alert`,
`state`, `thread_id`, `project`, `count`, `thread_title` (the raw title, empty
when the thread has none) and, for a grouped alert, `thread_titles` (one title
per line, an empty line for an untitled thread), plus `title` and `body` in
English. The app composes the notification itself, which is what lets a
foreground alert for the thread already on screen never appear in the tray at
all, and lets a running turn become an ongoing Live Update. `working` is sent to
Android phones only; iOS is never sent it.

The Android app words the alert in the phone's language (English, Simplified
Chinese, Spanish or Japanese, and English for any other) from the state,
project, count and titles: "h4rn · 已完成", "Requiere aprobación", "エージェント
3 件が対応待ち". The notification channel's name and description follow the
phone's language too, and are renamed when it changes. The English `title`
and `body` are what an app from before this change shows, and what a newer
app shows when a relay from before this change sends no `thread_title`. A
group too large to repeat its titles within FCM's 4096-byte limit is sent
without `thread_titles`; the phone then shows the English list in `body`.
The words live in Android string resources (`push_strings.xml`), described in
[i18n.md](i18n.md#phone-alerts).

**iOS** receives an APNs alert. The `aps` block carries Connect's English
title and body, `sound`, and `mutable-content: 1`; the only custom keys
are the fixed record's `h4rn_alert`, `state`, `thread_id` and `count`. The title
and body are not repeated as custom data. A grouped alert has an empty
`thread_id`, so a tap opens h4rn. `apns-expiration` gives the alert the same
300-second lifetime as the Android message.

## Operator setup: Connect

Push is off per platform until that platform's credentials are configured.
Connect reports each state in `/v1/push/status`; it never pretends to send. A
relay can carry Android, iOS or both.

### Android: Firebase

1. Create a Firebase project and enable the Firebase Cloud Messaging API.
2. Register the Android application id `dev.h4rn.app` in that project.
3. Create a service account with permission to send FCM messages and download
   its JSON key. Keep it outside the repository and the app bundle.
4. Give it to the relay, either as the JSON itself or as a path to the file:

   ```sh
   H4RN_CONNECT_FCM_SERVICE_ACCOUNT=/etc/h4rn/firebase-service-account.json
   ```

   `H4RN_CONNECT_FCM_TOKEN_URL` and `H4RN_CONNECT_FCM_URL` default to Google's
   endpoints and exist so tests can point at a mock server.
5. Restart `h4rn-connect`. The migration creates `push_devices` and
   `push_thread_states`; no manual SQL is needed.

The sender signs an RS256 JWT-bearer assertion with the service account key,
caches the OAuth2 access token until shortly before it expires, and posts a
data message to `https://fcm.googleapis.com/v1/projects/<project>/messages:send`.
A token is deleted only when FCM's error body names `UNREGISTERED` in its
`details[].errorCode`; 429 and 5xx responses are retried with backoff. A bare
top-level `NOT_FOUND`, or a 404 with no such detail, is not enough evidence: a
wrong `H4RN_CONNECT_FCM_URL` or project id also answers 404, and deleting every
token on that would silence the whole account because of an operator typo.
Those answers leave the token in place and only log the failure.

### iOS: APNs

iOS is sent to Apple directly, not through Firebase. The phone app has no
Firebase SDK, so the token it registers is an APNs device token; Connect talks
to APNs itself with the operator's own auth key.

1. In the Apple Developer portal, create an **APNs Auth Key** (`.p8`) for the
   team that owns `dev.h4rn.app`. Download it once; Apple does not show it
   again. Note its **Key ID** and the **Team ID**.
2. Give all three to the relay, either as the PEM itself or as a path to the
   file:

   ```sh
   H4RN_CONNECT_APNS_KEY=/etc/h4rn/AuthKey_KEYID12345.p8
   H4RN_CONNECT_APNS_KEY_ID=KEYID12345
   H4RN_CONNECT_APNS_TEAM_ID=TEAM123456
   ```

   When none of the three is set, iOS push is off and the relay starts normally.
   A partial set logs a warning naming all three variables, and a key that
   cannot be parsed logs an error; either way the relay still starts.
   `H4RN_CONNECT_APNS_TOPIC` defaults to `dev.h4rn.app`,
   and `H4RN_CONNECT_APNS_URL` defaults to `https://api.push.apple.com`; a
   development build of the app must use
   `H4RN_CONNECT_APNS_URL=https://api.sandbox.push.apple.com` because Apple
   keeps the sandbox and production device sets separate.
3. Restart `h4rn-connect`. The same `push_devices` migration records each
   phone's platform.

The sender signs an ES256 provider token with the key, caches it for 50 minutes
(Apple's limit is one hour), and posts the alert over HTTP/2 to
`/3/device/<token>`. A token is deleted only on a `410` or an explicit
`Unregistered` reason; `BadDeviceToken` is deliberately not enough, because a
development build pointed at the production host answers with it for a token
that is still valid elsewhere. 429 and 5xx responses are retried with backoff,
and an expired provider token is re-signed once. An `.p8` key is a credential:
it belongs only on the relay, never in the app bundle, a CI environment or this
repository.

A device row belongs to the account that registered it. Registration accepts
only the caller's own Connect device id, rotation replaces the old token,
signing out removes that phone's registration, revoking a device deletes its
registration in the same transaction, and account deletion removes all of a
user's rows. A revoked device is never selected as a push target even if a
registration row somehow survives.

## Operator setup: the Android build

`google-services.json` is a build secret and is never committed. Provide it at
build time:

```sh
H4RN_ANDROID_GOOGLE_SERVICES_FILE=/absolute/path/google-services.json \
  scripts/mobile-android.sh build
```

The script copies it into the generated, git-ignored `gen/android/app/` tree,
where the Google Services plugin reads it. Without that variable nothing is
copied, the plugin is not applied, and the app still builds: Firebase has no
project to initialise, so Settings reports alerts as unavailable. CI builds
the same way and needs no Firebase credentials.

The Firebase SDK is compiled into every build, so a normal release can enable
alerts without a different native module. The service-account key belongs only
on the relay, never in the APK or an EAS/build environment for the app.

## Operator setup: the iOS build

The iOS app needs no plist and no Firebase project. `apps/mobile/native/ios`
holds a `H4rnPush` Swift target, linked from `apps/mobile/src-tauri/build.rs`,
that registers for remote notifications and exposes the same commands as
Android (`push_status`, `push_request_permission`, `push_token`,
`push_set_foreground_thread`, `push_set_enabled`, plus
`push_take_pending_link` for a tap that cold-started the app). Every launch
asks APNs for a fresh token when permission is granted and waits for that
answer; the cached token is only a fallback after a few seconds. That is what
replaces a token Apple retired, for example after a phone was restored from a
backup. The app puts the current token on Connect when it signs in and whenever
it returns to the foreground, so a changed token reaches the relay.

What the build still needs from the owner is Apple's usual signing material:

1. The `dev.h4rn.app` bundle id with the **Push Notifications** capability
   enabled, and a provisioning profile that includes the
   `aps-environment` entitlement. `Release` builds use `production`; a debug
   build installed from Xcode uses `development` and must talk to the sandbox
   APNs host above.
2. `tauri ios init` generates the Xcode project and its entitlements file under
   the git-ignored `apps/mobile/src-tauri/gen/apple` tree. The target's
   `.entitlements` file there must also carry `aps-environment`; the generated
   tree is recreated by `ios init`, so add it in your own build setup rather
   than expecting a source change here to survive. Without it, registration
   fails and Settings reports that no APNs token arrived.
3. A device, not the simulator: the simulator cannot receive a remote push
   notification, so Settings on a simulator accepts the switch and then reports
   that no APNs token arrived. This is a limitation of the simulator, not of the
   relay.

CI compiles the simulator build with no Apple credentials at all; it only
verifies the Swift and Rust compile. The Swift package's XCTest target
(`Tests/H4rnPushTests`) is checked in but no workflow runs it yet, so treat
those tests as unverified until a macOS job runs `swift test` or an
`xcodebuild test` against a simulator.

**No Notification Service Extension ships yet.** iOS cannot run app code for an
alert while the app is suspended, so the extension that would re-check the
local switch before showing a suspended app's alert is not part of this
release: adding one needs its own Xcode target, and the generated iOS project
is not something this repository can add a target to. The switch-off removal
from Connect is therefore the guarantee while the app is suspended, and it
holds only once the removal reaches Connect: a switched-off or signed-out
iPhone that cannot reach the relay may still show alerts until the pending
removal succeeds at the next sign-in. The in-app delegate enforces the same
rule (switch off, exact thread open, grouped alert) whenever the app is
running. Do not rely on the local flag alone to silence a phone whose removal
never reached Connect.

## Limits

- Live Updates are Android only. iOS would need an ActivityKit widget extension
  with its own Xcode target, which the generated iOS project cannot gain here,
  so the iPhone app offers no equivalent and is not sent the `working` record.
- An Android ongoing notification is removed by its final alert, by the app's
  thread list, or by the switch-off/sign-out cancel. If the app is not running
  and the turn ends without an alert (a Stop, or a grouped finish), the entry
  stays until the app is next opened and has fetched the thread list of that
  thread's machine; no frame can remove it sooner. An entry for a thread the
  connected machine does not list (another machine's, or one deleted since) is
  kept, and goes away with that thread's next final alert, with the switch, or
  on sign-out.
- Android needs Google Play services. A device without them cannot receive FCM.
- iOS alerts are in English whatever the phone's language. The system shows
  the `aps` alert before any h4rn code runs, and wording it on the phone needs
  a Notification Service Extension (its own Xcode target) or localized strings
  at the root of the app bundle. The repository has no step that adds either to
  the generated iOS project yet (ADR-054).
- iOS needs a signed build with the push entitlement, installed on a real
  device, and an operator-configured APNs key on the relay. A simulator cannot
  receive a remote push, and a development build must use the sandbox APNs host.
- If the app is force-stopped in Android Settings, delivery stops until it is
  opened again. Android's battery modes can also delay a message.
- Turning the app switch off removes the registration; if Connect cannot be
  reached, the app retries until it confirms the row is gone. On Android the
  phone stays silent meanwhile; on iOS see the next two bullets. Turning off
  the daemon switch stops publishing for every phone at once. Revoking the
  phone from the desktop removes its registration too.
- On iOS, an alert delivered while the app is suspended is shown by the system
  before any h4rn code runs. Without a Notification Service Extension the local
  switch cannot suppress it; removal from Connect, and the in-app delegate, are
  what keep a switched-off phone quiet. So a signed-out or switched-off iPhone
  that cannot reach Connect may still show alerts while suspended, until the
  pending removal succeeds at the next sign-in. There is no lock-screen
  guarantee for that case on iOS; the equivalent guarantee is Android-only.
- On iOS, a tap that launches the app is buffered by the native delegate,
  which is installed during launch as Apple requires, and read once by the app
  during boot, so it opens its thread after a cold start as well as while the
  app is running. This path is compiled in CI but has only been checked by
  reading, not on a device; confirm it on a real iPhone before relying on it.
- A thread moved to Done by hand is a placement, not a finished turn, so it
  does not alert. A turn you stopped yourself does not alert either.
- A turn stopped by a usage, token or turn limit asks for you only when
  auto-resume is off; otherwise the daemon continues by itself.
- Subagent threads are not published separately; their parent thread's
  approvals are.
- Connect keeps no conversation history for push, and the notification text is
  produced from the six fields above, by Connect in English and by the Android
  app in the phone's language.
