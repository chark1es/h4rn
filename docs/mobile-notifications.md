# Mobile notifications

Android can alert you when an agent finishes, fails, waits for an approval, asks
a question or stops at a limit. Tap the alert to open that thread. Delivery goes
through Connect only: a direct or Tailscale pairing is not enough, because the
daemon does not hold a phone's push token and Google cannot reach a LAN address.

iOS alerts are not part of this feature; Android is first.

## Turning alerts on

1. Sign in to Connect in h4rn and link the host that runs the agents.
2. On that host, open **Settings → Notifications** and turn on **Send agent
   alerts to my phone through Connect**. This is a machine setting: it is off
   until you turn it on, and turning it off stops the next publish. Turning it
   off works from any client, even one that is not signed in to Connect, because
   the setting belongs to the machine that publishes.
3. In the Android app, open **Settings → Notifications → Phone alerts** and
   turn on **Device notifications**. Android asks for notification permission
   the first time (Android 13 and newer). The row shows the real state: the OS
   permission, whether this phone registered with Connect (not whether another
   phone on the account did), and whether the Connect server has a push sender
   configured. A status call that fails is reported as unknown, not as a missing
   sender.

Alerts are quiet only when the app is in the foreground **and** that exact
thread is on screen. Every other thread still alerts, as does the same thread
while the app is in the background.

## What leaves the daemon

Each alert carries exactly this record:

| Field | Example | Notes |
|---|---|---|
| thread id | `th_0192f0…` | Opens the thread on tap |
| thread title | `Fix the flaky login test` | As shown in the sidebar |
| project name | `h4rn` | Folder name, not the full path |
| state | `completed`, `failed`, `approval`, `input`, `limit` | One of five transitions |
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
alert. The count is of threads, not raw transitions, so one agent that raises
the same hand twice inside the group window is still one agent.

## Operator setup: Connect

Push is off when no Firebase service account is configured. Connect reports
that state in `/v1/push/status`; it never pretends to send.

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
A token that FCM reports as `UNREGISTERED` or `NOT_FOUND` in its error body is
deleted; 429 and 5xx responses are retried with backoff. A bare 404 is not
enough evidence: a wrong `H4RN_CONNECT_FCM_URL` or project id also answers 404,
and deleting every token on that would silence the whole account because of an
operator typo. Those answers leave the token in place and only log the failure.

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

## Limits

- Android needs Google Play services. A device without them cannot receive FCM.
- If the app is force-stopped in Android Settings, delivery stops until it is
  opened again. Android's battery modes can also delay a message.
- Turning the app switch off removes the registration; turning off the daemon
  switch stops publishing for every phone at once. Revoking the phone from the
  desktop removes its registration too.
- A thread moved to Done by hand is a placement, not a finished turn, so it
  does not alert. A turn you stopped yourself does not alert either.
- A turn stopped by a usage, token or turn limit asks for you only when
  auto-resume is off; otherwise the daemon continues by itself.
- Subagent threads are not published separately; their parent thread's
  approvals are.
- Connect keeps no conversation history for push, and the notification text is
  produced from the six fields above.
