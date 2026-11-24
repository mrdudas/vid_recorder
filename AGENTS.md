# vid_recorder – projekt irányelvek

Rögzítő alkalmazás Allied Vision kamerához. A cél, hogy a kamera jelét a lehető legnagyobb pontossággal rögzítsük.

## Hardver és platform

- **Kamera:** Allied Vision, **USB**-n csatlakozik, **USB3 Vision** szabvány szerint.
  - **Nem használunk Vimba SDK-t** (döntés: 2026-11-24). Ok: a Vimba X macOS-es változata csak Apple Siliconon fut, és így minden platformon egységes a kód.
  - A kamerával közvetlenül a USB3 Vision / GenICam protokollon beszélünk, Rust könyvtárral (`cameleon`, libusb-n keresztül).
  - Windowson a kamerához WinUSB meghajtó kell (pl. Zadiggal telepítve).
  - Linuxon a `linux/99-vid-recorder.rules` udev szabály kell: root nélküli hozzáférést ad, és a 16 MB-os `usbfs_memory_mb` korlátot 1000-re emeli, különben képkockák veszhetnek el. A program a kamera megnyitásakor ellenőrzi a korlátot, és ha kell, megemeli (root nélkül `pkexec`-kel kér jogot).
  - A kamerakezelés egy saját interfész mögött van, így a USB3 Vision réteg cserélhető.
- **UVC kamerák is használhatók** (webkamera, UVC-kompatibilis panelkamera) a `nokhwa` könyvtárral: Macen AVFoundation, Windowson Media Foundation, Linuxon V4L2.
  - Formátumok: MJPEG, YUYV, NV12, szürke (Mono8).
  - **Az MJPEG változatlanul kerül a fájlba**, nincs kicsomagolás és újrakódolás. A YUYV és az NV12 veszteségmentesen kódolódik.
  - A kódolót mindig az első ténylegesen beérkező kép formátuma határozza meg. Ha a kamera mást ad, mint amit a szcenárió kért, a program jelzi. Pl. a Mac beépített kamerája MJPEG kérésre is YUYV-t ad.
  - A UVC nem ad képkocka-sorszámot, ezért a program a beérkezés sorrendjében számozza a képeket. **UVC-nél a kamera oldali képkockavesztést nem lehet kimutatni.** Az időbélyeg a rendszer felvételi ideje (Unix epoch, ns), ha a rendszer megadja.
  - Az expozíciót és az erősítést UVC-nél a program egyelőre nem állítja.
- **Eszközválasztás:** minden szcenárió tárolja, melyik kamerát használja (`device`: `u3v`, `uvc` + név, vagy `sim`). Szcenárióváltáskor a program a kamerát is átváltja.
- **Szenzor:** Sony, ezért **S-Log** kell. Az S-Log görbék/LUT-ok **külön könyvtárban** vannak.
- **Platformok:** macOS, Windows és Linux. Mindhárom támogatott, egyik sem másodrangú.

## Rögzítés – kötelező követelmények

- **Nem veszhet el képkocka.** A rögzített anyagnak minden képkockát tartalmaznia kell. A rögzítési útvonal élvez elsőbbséget mindennel szemben.
- **Élőkép legfeljebb 25 fps-sel.** A megjelenítés külön szálon fut, és nem lassíthatja vagy blokkolhatja a rögzítést. Ha kell, az élőkép dobja el a képkockákat, a felvétel soha.
- **Formátum:** MKV vagy hasonló konténer, **minimális tömörítéssel**, mert az anyagot később dolgozzuk fel.
- **A nyers kép LUT nélkül, veszteségmentesen (bitre pontosan) kerül mentésre.** Az `auto` kódoló csak veszteségmentes profilt választ: 8 biten `ffvhuff`, efölött `ffv1`. A hardveres kódolók veszteségesek, ezért csak kézzel választhatók.
- **S-Log LUT** (`.cube` fájlok a `slog/` mappában): csak az **élőképen** használjuk, ott elég a 8 bit. Később az **exportnál** is kell majd. A felvételre soha nem kerül LUT.
- **Hardveres kódolás, ha van:** Intel (QSV), NVIDIA (NVENC) vagy AMD (AMF). Macen ezt a VideoToolbox adja (Intel Macen az Intel hardverét használja). Ha egyik sem érhető el, szoftveres kódolás az alternatíva.
  - A kódolást egy külön indított **ffmpeg** folyamat végzi, ennek a bemenetére írjuk a nyers képkockákat. Nem linkeljük a programba, így az ffmpeg GPL-es buildje nem érinti a licencünket.
- **Időbélyegek:** minden képkockához tartozik időbélyeg. Ezek egy CSV-be kerülnek, amelynek a neve **megegyezik a felvétel nevével** (pl. `felvetel01.mkv` → `felvetel01.csv`).
  - Formátum: `timestamp,frameID`, soronként egy képkocka.

## Export

- `vid_recorder export <felvétel> <kimenet> <módszerek> [opciók]`. A felvételt soha nem módosítja. A valós képkockasebességet az időbélyeg-CSV-ből veszi, ha van.
- **Natív export** (`native`): a felvétel feldolgozás nélkül. Beállítható a képkockasebesség (`fps=`, `slow`) és a tömörítés (`codec=h264|hevc|prores|ffv1`). Az FFV1 bitre pontos, és megtartja az eredeti bitmélységet.
- **Mozgás- és színnagyítás:** fázisalapú (Riesz) és tremor, lineáris Euler-féle (szűrt és szűretlen), színnagyítás a bőrön. A kimenet rács: az eredeti, majd sávonként egy panel.
  - A módszerek a G_matyas-elemzésből (2026. október) vannak Rustba átírva. A Python-referencia itt található: `/mnt/data/Database/G_matyas/scripts/`.
- A Bayer-felvétel a fájlba szürke mozaikként kerül. A recorder a `PIXEL_FORMAT` címkébe írja a formátumot, ebből az export felismeri, és demozaikolja (ffmpeg).

## Felhasználói felület

### Beállító oldal
- Itt állítjuk össze a **szcenáriókat** (kamerabeállítás-készleteket).
- Élőképen ellenőrizhető, milyen képet ad a kamera az adott beállításokkal.
- Minden beállítás elmenthető, **JSON** formátumban.

### Főoldal
- Itt lehet kiválasztani a mentett szcenáriókat. **Szcenárióváltáskor a kamera azonnal átáll** az új beállításokra.
- **Record Start** és **Record End** gomb.
- Élőkép (max. 25 fps).

## Indítás és frissítés

- **A programot az indítószkripttel indítjuk:** `./run.sh` (macOS/Linux) vagy `.\run.ps1` (Windows). Az argumentumokat továbbadja, pl. `./run.sh record-test 5`. Ha nincs hálózat, a frissítés nem sikerül, vagy helyi módosítás van, a meglévő verzió indul.
  - **Fejlesztő gép** (git-klón és telepített Rust): `git pull --ff-only`, fordítás, ha bármi változott, aztán indítás.
  - **Felhasználói gép** (kiadásból telepítve): ha a GitHub legújabb kiadása eltér a `VERSION` fájlban lévőtől, letölti és kicsomagolja a helyére. Nem kell Rust és fordítás. Az ffmpeg-et viszont telepíteni kell.
- **Kiadás:** a `.github/workflows/release.yml` minden `v*` címkére lefordítja a programot macOS-re (arm64 és x86_64), Linuxra (x86_64) és Windowsra (x86_64), és GitHub Release-be teszi. Lépések: a `Cargo.toml` verzióját átírni (pl. `0.2.0`), commitolni, aztán `git tag v0.2.0 && git push origin v0.2.0`. A címkének egyeznie kell a `Cargo.toml` verziójával. A repo nyilvános (2026-11-24), hogy a kiadás belépés nélkül letölthető legyen.
- **Az agentek munkakezdéskor frissítenek:** a `.claude/settings.json` SessionStart hookja minden Claude-munkamenet indulásakor lefuttatja a `.claude/hooks/git-pull.sh`-t. Helyi módosításnál nem húz le semmit, és sosem blokkolja a munkamenetet.

## Kódolási elvek

- **Hatékony kód, semmi fölösleges extra.** Csak azt építjük meg, ami kell.
- Nyelv: **Rust**, ha megoldható, vagy egy hasonlóan hatékony, natív nyelv.
- Egyszerű, átlátható felépítés. Nem kellenek felesleges absztrakciók és fancy UI.
