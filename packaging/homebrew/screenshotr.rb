cask "screenshotr" do
  version "__VERSION__"
  sha256 "__SHA256__"

  url "https://github.com/__GITHUB_REPO__/releases/download/v#{version}/Screenshotr_#{version}_aarch64.dmg"
  name "Screenshotr"
  desc "Capture a screenshot or file and get a link from your own storage"
  homepage "https://github.com/__GITHUB_REPO__"

  livecheck do
    url :url
    strategy :github_latest
  end

  # The app updates itself, so Homebrew leaves upgrades to it.
  auto_updates true
  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "Screenshotr.app"

  zap trash: [
    "~/Library/Application Support/ca.aahmed.screenshotr",
    "~/Library/Caches/ca.aahmed.screenshotr",
    "~/Library/LaunchAgents/ca.aahmed.screenshotr.plist",
    "~/Library/Preferences/ca.aahmed.screenshotr.plist",
    "~/Library/WebKit/ca.aahmed.screenshotr",
  ]
end
