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

  override fun onDestroy() {
    h4rnWebView = null
    super.onDestroy()
  }
}
