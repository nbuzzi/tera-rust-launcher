Place the Spanish DataCenter here:

  DataCenter_Final_EUR.dat

(same filename TERA expects; ~37 MB). The launcher will bundle it as a
resource and use it when the user clicks "Switch to Spanish".

For dev builds (cargo build --release without installer), the launcher
also looks for this file in:
  - <launcher_exe_dir>/resources/lang/DataCenter_Final_EUR.dat
  - <src-tauri>/resources/lang/DataCenter_Final_EUR.dat  (this folder)
