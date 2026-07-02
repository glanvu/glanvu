class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.8.0/glanvu-0.8.0-linux-x86_64.tar.gz"
    sha256 "092a8b53785d15972462aa124634dacfe816f647462ec066148d6402014bcfad"
    version "0.8.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.8.0/Glanvu-0.8.0-macos-arm64.zip"
      sha256 "2aa7ea359c72f2c01c650fc043787d67220c4c2cf28d88b311821e5c902af8bc"
      version "0.8.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.8.0/Glanvu-0.8.0-macos-x86_64.zip"
      sha256 "89576f6de3d93a4462c05838f9a7225cb0056c117c973d98d42ed0f65af3373b"
      version "0.8.0"
    end
  end

  def install
    if OS.mac?
      bin.install "Glanvu.app/Contents/MacOS/glanvu"
    else
      bin.install "glanvu"
    end
  end

  test do
    system bin/"glanvu", "--help"
  end
end
