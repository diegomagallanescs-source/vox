Vox - local push-to-talk dictation for Windows
=============================================

Hold a key, talk, let go, and what you said is typed into whatever window you were
using. Everything runs on your own PC - no account, no internet, nothing uploaded.

GETTING STARTED
---------------
1. Unzip this folder anywhere (Desktop is fine). Keep voxd.exe and the models folder
   together - Vox looks for the model next to itself.
2. Run voxd.exe.
   Windows will probably say "Windows protected your PC" because this isn't code-signed.
   Click "More info", then "Run anyway".
3. The settings window opens and a small blue microphone appears in your system tray
   (bottom-right, possibly hidden under the ^ arrow).
4. Click "Change..." next to the hotkey and press the key you want. If you're not sure,
   click one of the suggestions - Right Ctrl is a good default.
5. Pick your microphone.
6. Open Notepad, hold your key, say something, let go. Wait for the tick before you
   start talking.

Closing the window leaves Vox running in the tray. Click the tray icon to bring it
back, right-click it to quit.

REQUIREMENTS
------------
- Windows 10 or 11, 64-bit
- A CPU from roughly 2013 or later (needs AVX2)
- About 1 GB of free RAM while running

A NOTE ON BLUETOOTH HEADPHONES
------------------------------
If you dictate through Bluetooth earbuds or headphones, your music will sound muffled
while you talk. Bluetooth cannot carry high-quality audio and a microphone at the same
time, so Windows drops the headset into call mode. This affects every app, not just
Vox. Using any wired or USB microphone avoids it completely, and gives noticeably
better accuracy too.

SPEED AND ACCURACY
------------------
This build includes base.en, which turns about 3 seconds of speech into text in about
a fifth of a second on a modern desktop. If you want better accuracy and don't mind
waiting a little longer, download a bigger model and drop it in the models folder:

  https://huggingface.co/ggerganov/whisper.cpp

  ggml-small.en-q5_1.bin        190 MB, a bit more accurate, ~0.7s
  ggml-large-v3-turbo-q5_0.bin  550 MB, best accuracy, needs a GPU build to be usable

Then choose it in the Speech engine card.

TROUBLESHOOTING
---------------
Nothing happens when I press the key
  Another program may have claimed it. Try one of the suggested keys instead. Note
  that Ctrl+Alt+Del and Win+L are handled by Windows itself and cannot be used.

It types nothing, or the wrong thing
  Run vox.exe mic-test 4 from a terminal in this folder. It records for four seconds
  and prints what it heard, along with the input level. If the level says "silence",
  the wrong microphone is selected.

Where are the logs?
  %LOCALAPPDATA%\Vox\logs\voxd.log

Source code: https://github.com/diegomagallanescs-source/vox
