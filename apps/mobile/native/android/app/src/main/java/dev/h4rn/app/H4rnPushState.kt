package dev.h4rn.app

/** Whether the exact thread is on screen in the foreground. */
object H4rnPushState {
  @Volatile var foreground: Boolean = false
  @Volatile var threadId: String? = null
}

/**
 * Pending notification permission request.
 *
 * A Tauri command must answer only after the system dialog is gone, so the
 * shared object hands the result back from the activity's callback instead of
 * reporting a permission the person has not decided yet.
 */
object H4rnPushPermission {
  const val REQUEST_CODE = 4821
  private var pending: app.tauri.plugin.Invoke? = null

  fun ask(activity: android.app.Activity, invoke: app.tauri.plugin.Invoke) {
    pending?.reject("a notification permission request is already open")
    pending = invoke
    activity.requestPermissions(
      arrayOf(android.Manifest.permission.POST_NOTIFICATIONS),
      REQUEST_CODE,
    )
  }

  fun resolved() {
    val invoke = pending ?: return
    pending = null
    invoke.resolve()
  }
}
