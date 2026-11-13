# h4rn Homebrew cask (template).
#
# `packaging/scripts/update-manifests.sh` fills the version and DMG checksum
# and writes `packaging-out/homebrew/h4rn.rb`; submit that file to the tap
# (or a homebrew-cask PR) by hand.
cask "h4rn" do
  version "__VERSION__"
  sha256 "__DMG_SHA256__"

  url "https://github.com/chark1es/h4rn/releases/download/v__VERSION__/__DMG_NAME__",
      verified: "github.com/chark1es/h4rn/"
  name "h4rn"
  desc "Quiet-native orchestration for local coding agents"
  homepage "https://github.com/chark1es/h4rn"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :big_sur"

  app "h4rn.app"

  zap trash: [
    "~/Library/Application Support/dev.h4rn.desktop",
    "~/Library/Caches/dev.h4rn.desktop",
    "~/Library/Logs/dev.h4rn.desktop",
    "~/Library/Preferences/dev.h4rn.desktop.plist",
    "~/Library/Saved Application State/dev.h4rn.desktop.savedState",
  ]
end
