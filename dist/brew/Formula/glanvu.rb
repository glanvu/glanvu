class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.7.0/glanvu-0.7.0-linux-x86_64.tar.gz"
    sha256 "d5764f409fc4afeb1d5febf5f02ab0064c90bec9dd978fdaaeeb98253982882e"
    version "0.7.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.7.0/Glanvu-0.7.0-macos-arm64.zip"
      sha256 "d15f534d6a915bb4098892d1b2df2f2ca27aa89185321216a6525aee632d535a"
      version "0.7.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.7.0/Glanvu-0.7.0-macos-x86_64.zip"
      sha256 "70a8cf1b2c7eadef642def95be6f639115a2961cdb9f230db9d5c6279b04aba4"
      version "0.7.0"
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
