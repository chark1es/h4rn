package dev.h4rn.app

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge

/**
 * h4rn's Android entry point.
 *
 * Tauri ships `handleBackNavigation = false`, so the hardware back button would
 * close the app straight from a thread. We own the callback instead and map it
 * onto the WebView's own history, which is the same history the React router
 * writes through `history.pushState`. The React app already listens for
 * `popstate`, so back walks thread -> inbox exactly like the system gesture
 * users expect, and only exits once there is nothing left to pop.
 */
class MainActivity : TauriActivity() {
  private var h4rnWebView: WebView? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    h4rnWebView = webView
    onBackPressedDispatcher.addCallback(
      this,
      object : OnBackPressedCallback(true) {
        override fun handleOnBackPressed() {
          val view = h4rnWebView
          if (view != null && view.canGoBack()) {
            view.goBack()
          } else {
            isEnabled = false
            onBackPressedDispatcher.onBackPressed()
            isEnabled = true
          }
        }
      },
    )
  }

  override fun onStart() {
    super.onStart()
    // A foreground alert is quiet only for the thread on screen. The shared UI
    // updates the thread with `push_set_foreground_thread`; this flag says
    // whether the window is really in front.
    H4rnPushState.foreground = true
  }

  override fun onStop() {
    H4rnPushState.foreground = false
    super.onStop()
  }

  override fun onRequestPermissionsResult(
    requestCode: Int,
    permissions: Array<out String>,
    grantResults: IntArray,
  ) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (requestCode == H4rnPushPermission.REQUEST_CODE) {
      H4rnPushPermission.resolved()
    }
  }

  override fun onDestroy() {
    h4rnWebView = null
    H4rnPushState.foreground = false
    super.onDestroy()
  }
}
