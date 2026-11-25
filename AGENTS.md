# vid_recorder – projekt irányelvek

Rögzítő alkalmazás Allied Vision kamerához. A cél, hogy a kamera jelét a lehető legnagyobb pontossággal rögzítsük.

## Hardver és platform

- **Kamera:** Allied Vision, **USB**-n csatlakozik, **USB3 Vision** szabvány szerint.
  - **Nem használunk Vimba SDK-t** (döntés: 2026-11-24). Ok: a Vimba X macOS-es változata csak Apple Siliconon fut, és így minden platformon egységes a kód.
  - A kamerával közvetlenül a USB3 Vision / GenICam protokollon beszélünk, Rust könyvtárral (`cameleon`, libusb-n keresztül).
  - Windowson a kamerához WinUSB meghajtó kell (pl. Zadiggal telepítve).
  - Linuxon a `linux/70-vid-recorder.rules` udev szabály kell: root nélküli hozzáférést ad, és a 16 MB-os `usbfs_memory_mb` korlátot 1000-re emeli, különben képkockák veszhetnek el. A program a kamera megnyitásakor ellenőrzi a korlátot, és ha kell, megemeli (root nélkül `pkexec`-kel kér jogot).
  - A kamerakezelés egy saját interfész mögött van, így a USB3 Vision réteg cserélhető.
  - **Képfrekvencia:** a kamera bármilyen értéket elfogad, de ha a szenzor vagy a USB nem bírja, csendben lassabb lesz, vagy eldobja a képeket. A program ezért elutasítja azt a beállítást, amely meghaladja a kamera `AcquisitionFrameRate` maximumát (ez függ a formátumtól, a mérettől és az expozíciótól) vagy a `DeviceLinkThroughputLimit`-et.
  - **USB:** mérés szerint (Alvium 1800 U-291c, 2026-11) a kapcsolat hibátlanul átviszi azt, amit a kamera 1080p-n maximálisan ad (kb. 406 MB/s). A képkockavesztés oka a processzorterhelés volt: amikor a kódoló leköti a CPU-t, a kamera olvasása lemarad. Ezért fut az ffmpeg alacsonyabb prioritással, és ezért FFVHuff az alapértelmezett kódoló.
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
- **A nyers kép LUT nélkül, veszteségmentesen (bitre pontosan) kerül mentésre.** Az `auto` kódoló minden bitmélységen `ffvhuff`-ot választ. 8 bit fölött a mintákat változatlan 16 bites szavakként (`gray16le`) kapja meg, a valós bitmélységet a `PIXEL_FORMAT` címke adja meg, és az export ebből skáláz. Az FFVHuff 1080p 10 biten kb. 380 fps-sel kódol, az FFV1 csak kb. 66 fps-sel, de az FFV1 fájlja kb. harmadával kisebb. Az `ffv1` ezért csak kézzel választható, alacsonyabb adatsebességhez. A hardveres kódolók veszteségesek, ezért csak kézzel választhatók.
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
- **Elrendezés:** az ablak maximalizálva indul. Bal oldalt a beállítások, jobb oldalt az élőkép, amely kitölti a rendelkezésre álló helyet.
- **Kivágás (ROI):** az élőképen kihúzott téglalap lesz a kamera `Width/Height/OffsetX/OffsetY` értéke. A kamera lépésközeire kerekít, és azonnal kipróbálja. Kisebb magasság mellett nagyobb az elérhető fps. Az eltolás a tükrözés utáni képben értendő (SFNC).
- **Határok felmérése:** felvétel nélkül, kb. 2–3 s alatt kiolvassa a kamerából a szenzorméretet, a lépésközöket, az expozíció- és erősítéstartományt, valamint a legnagyobb fps-t formátumonként és magasságonként (a USB-korláttal együtt). A beállítóoldal első megnyitásakor magától lefut. Parancssorból: `limits <szcenárió|u3v>`. A 12 bites és a többi formátum közti váltás az Alviumon kb. 1,5 s, ezért a formátumok csoportosítva futnak.
- **Automatikus expozíció:** a szcenárió megadja, milyen tartományban állíthatja a kamera az expozíciót (`auto_exposure_range`, µs) és az erősítést (`auto_gain_range`, dB). A felső expozícióhatár legfeljebb a képkocka idejének 95%-a lehet. Az fps-t a program az alsó határnál ellenőrzi.
- **Felvételi teszt:** a szerkesztett szcenáriót a megadott ideig rögzíti, és megmutatja, veszett-e el képkocka, illetve bírja-e a kódoló a tempót. A tesztfájlokat utána törli.
- **Kamera felderítése:** a kamerától lekérdezi a módokat, és mindegyiket lefuttatja a felvételi teszttel. Ha a kódoló túl lassú, tömörítetlenül (`raw`) is kipróbálja. Parancssorból: `probe <szcenárió|sim|u3v> [mp]`.
  - **USB3 Vision:** a GenICam adja meg az elérhető pixelformátumokat, a legnagyobb méretet és formátumonként a legnagyobb fps-t, az expozíció és az extra beállítások alapján.
  - **UVC:** a rendszer listája alapján. Macen a nokhwa csak a formátumot adja meg, ezért ott a program 1080p, 720p és 480p méretet próbál 30 fps-sel.

### Felvételek és export
- A szcenáriók kimeneti mappáiban és a kézzel hozzáadott mappákban (`recording_folders.txt`, nincs verziókezelve) lévő felvételek listája, az `export/` almappák tartalmával együtt. Látszik a méret, a hossz és az fps (az időbélyeg-CSV-ből), valamint a lemez szabad helye.
- **Megtekintés:** a név kattintására jobb oldalt megnyílik a lejátszó. Csúszkával és léptetőgombokkal böngészhető, az ffmpeg háttérszálon dekódol egy-egy képkockát. A nyers Bayer felvétel ugyanúgy jelenik meg, mint az élőkép, képenkénti automatikus fehéregyensúllyal és szintekkel. A valós időt a CSV-ből számolja, mert a konténer fps-e eltérhet.
- **Export** a kijelölt felvételekre. Ugyanaz, mint a parancssori `export`, külön folyamatban, alacsony prioritással, egyszerre egy, sorban. A kimenet a felvétel mappájában az `export/` almappába kerül. Fajtái:
  - **Archív** (`archive`): a nyers Bayer FFV1-gyel, bitre pontosan, kb. 20%-kal kisebb, mint az FFVHuff. A CSV is mellé kerül. Csak akkor sikeres, ha a képkockaszám egyezik.
  - **Natív videó**: demozaikolt, választható tömörítéssel.
  - **Mozgás- és színnagyítás**: a kiválasztott módszerek egy rácsban.
- **Törlés** a hely felszabadítására: kézzel, megerősítéssel, vagy választhatóan sikeres export után automatikusan. Felvétel vagy export alatt álló fájl nem törölhető.

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
