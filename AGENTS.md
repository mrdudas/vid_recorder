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
  - **Hibatűrés** (`Engine::poll`, az UI és a `record` parancs is hívja):
    - ha a kamera leválik, 3 s-ig nem küld képet, vagy induláskor nincs meg, a program másodpercenként újranyitja;
    - ha a kamera csatlakozik, de nem küld képet, előbb GenICam `DeviceReset`-et kap;
    - **a futó felvétel nem áll le**, a kamera visszatérése után ugyanabban a fájlban folytatódik. A CSV `frameID` és `timestamp` értékei a kiesésen át folytatódnak, a hiányzó képkockák sorszám-ugrásként látszanak, és elveszettnek számítanak;
    - a leállítás legfeljebb 2 s-ot vár egy beragadt kameraszálra.
  - **Tesztelésnél tilos a USB-portot letiltani** (`usbN-portM/disable`): az Alvium ettől megakadt, és szoftveresen (vezérlő-újraindítással) sem jött vissza, csak kihúzással (2026-11-25). A kihúzást a felhasználónak kell kipróbálnia.
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
- **Kivágás (ROI):** az élőkép (a beállítóoldalon és a főoldalon is) mindig a teljes szenzort mutatja, és a kamera kivágott képe a helyén látszik. A kivágás egy sárga téglalap: belsejét húzva mozgatható, oldalait és sarkait húzva átméretezhető, mellette húzva új rajzolható. Elengedéskor a program a kamera lépésközeire kerekíti, legalább a kamera legkisebb képméretére (`WidthMin/HeightMin`) állítja, és azonnal kipróbálja. Kisebb magasság mellett nagyobb az elérhető fps. Az eltolás a tükrözés utáni képben értendő (SFNC).
  - **Az Alvium 1800 U-291c kivágási szabályai** (mérve 2026-11-25, BayerRG8/10p/12p esetén azonosak):
    - szélesség 8-cal osztható, legalább 8; magasság páros, legalább 8; mindkét eltolás 8-cal osztható;
    - a használható terület tükrözés nélkül 1944×1472. `ReverseX` mellett 1936 a szélesség, `ReverseY` mellett 1470 a magasság (a Bayer-minta megtartásához egy oszlop- vagy sorpár kiesik);
    - legnagyobb eltolás = (terület − méret), 8-ra lefelé kerekítve. A jobb és az alsó szélt ezért csak akkor éri el a kivágás, ha a különbség 8-cal osztható;
    - ezen belül minden kombinációt elfogad (60 véletlen mintából 60). A program a vásznat és a határokat minden beállításkor a kamerától olvassa ki.
  - **A tükrözés átállítása után a kamera néhány ms-ig még a régi határokkal dolgozik.** Ha azonnal utána a teljes magasságot írjuk, visszautasítja.
  - **Ha az Alvium elutasít egy írást, vagy olyan funkciót olvasunk, amely nincs meg rajta (pl. `DecimationHorizontal`), utána minden írást elutasít (GenCp InvalidParameter), amíg újra meg nem nyitjuk.** Ezért a program írás előtt ellenőrzi az egész értékek tartományát és lépésközét (ez a kiolvasás a fenti várakozást is megoldja), I/O hiba után pedig újranyitja a kamerát. Az el nem fogadott kivágás helyett visszaáll az előzőre. Nem létező funkciót nem szabad olvasni.
- **Határok felmérése:** felvétel nélkül, kb. 2–3 s alatt kiolvassa a kamerából a szenzorméretet, a lépésközöket, az expozíció- és erősítéstartományt, valamint a legnagyobb fps-t formátumonként és magasságonként (a USB-korláttal együtt). A beállítóoldal első megnyitásakor magától lefut. Parancssorból: `limits <szcenárió|u3v>`. A 12 bites és a többi formátum közti váltás az Alviumon kb. 1,5 s, ezért a formátumok csoportosítva futnak.
- **Automatikus expozíció:** a szcenárió megadja, milyen tartományban állíthatja a kamera az expozíciót (`auto_exposure_range`, µs) és az erősítést (`auto_gain_range`, dB). A felső expozícióhatár legfeljebb a képkocka idejének 95%-a lehet. Az fps-t a program az alsó határnál ellenőrzi.
- **Felvételi teszt:** a szerkesztett szcenáriót a megadott ideig rögzíti, és megmutatja, veszett-e el képkocka, illetve bírja-e a kódoló a tempót. A tesztfájlokat utána törli.
- **Kamera felderítése:** a kamerától lekérdezi a módokat, és mindegyiket lefuttatja a felvételi teszttel. Ha a kódoló túl lassú, tömörítetlenül (`raw`) is kipróbálja. Parancssorból: `probe <szcenárió|sim|u3v> [mp]`.
  - **USB3 Vision:** a GenICam adja meg az elérhető pixelformátumokat, a legnagyobb méretet és formátumonként a legnagyobb fps-t, az expozíció és az extra beállítások alapján.
  - **UVC:** a rendszer listája alapján. Ha a rendszer nem adja meg a módokat (Macen a nokhwa csak a formátumot), a program kitapogatja őket: minden formátumot és gyakori méretet megnyit, és csak azt tartja meg, amit a kamera ténylegesen küld (formátum és méret az első képből). A tesztek a legnagyobb fps-sel kezdenek, és ha a kamera nem bírja, a következő kisebbel próbálkoznak (60 → 30 → 15).
  - A beállítóoldalon a **Kameramód** listában csak a kamera saját módjai vannak; a teszten elbukott módok és a kamerán nem létező formátumok szürkék. A **„A kamera most küld”** sor a beérkező képekből mért formátumot, méretet és fps-t mutatja, és pirossal jelzi, ha eltér a kérttől (pl. Mono8 helyett YUYV).
- **Szoftveres kivágás:** ha a kamera nem tud vágni (UVC, vagy írhatatlan `Width/Height/Offset`), a kamera a `capture_size` méretben fut, és a program ugyanúgy kivágja a képet, mint a kamera tenné. A felvétel és az élőkép is a kivágott képet kapja. Bayer, YUYV és NV12 esetén a lépésköz 2 (megmarad a színminta); MJPEG nem vágható.

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
- Nagy **REC** gomb. Felvétel közben **Szünet/Folytatás** és **Stop**.
  - Szünet alatt nem íródik kép. A CSV-ben a szünet a `frameID` és az időbélyeg ugrásaként látszik. A felvétel hossza és a valós fps a medián képközből számolódik, ezért a szünet nem torzítja.
- Felvétel közben az élőkép bal felső sarkában villogó piros **● REC** látszik, szünetben sárga **⏸ PAUSE**, mellette a rögzített idő (a szünetek nélkül), mint egy videokamerán.
- Labor operátoroknak készül: szcenárió, felvétel, Stop, aztán **Export** a leállított felvételre. Az export módját a szcenárió adja meg (`export`: archív, natív videó vagy nagyítás).
- Alul állapotsor minden oldalon: egy lámpa mutatja, rendben van-e a kamera és a felvétel. A technikai részletek és az élőkép beállításai csukott részben vannak.
- **Felvétel közben csak a főoldal érhető el**, a Beállítások és a Felvételek menü le van tiltva.
- Élőkép (max. 25 fps).
- **Átlagolás és igazítás** (az élőkép beállításaiban, az „Automatikus fényerő” alatt; csak az élőképre hat, a felvétel nyers marad):
  - Az élőkép több kép átlaga. A képszám megadható, vagy automatikus: addig vesz hozzá képet, amíg egy újabb kevesebb mint 0,2%-kal változtat az átlagon, utána ennyi kép futó átlaga (legfeljebb 1000). Szcenárióváltáskor vagy a beállítás változásakor újrakezdi.
  - **Igazítás** (ki-be kapcsolható): minden képet eltolással és forgatással az átlaghoz igazít (képpiramis, Gauss–Newton, durva eltoláskereséssel), pl. csillagvizsgálónál. 1080p színes képen kb. 44 ms/kép, külön szálon; ha nem bírja, képeket hagy ki az átlagból.
  - Az átlag a beérkező élőkép-képekből (max. 25 fps) készül, nem minden kamerakockából.
  - **Átlagolt kép mentése:** 16 bites lineáris PNG (`atlag_<dátum>.png`) a szcenárió mappájába, LUT, fehéregyensúly és szintek nélkül. A Bayer-kép 2×2-es szuperpixelekből áll, ezért fél felbontású.
- **Kivágás a főoldalon is**, ugyanúgy, mint a beállítóoldalon, de csak a futó szcenárióra hat, nem kerül mentésre. Szcenárióváltásig vagy „A szcenárió kivágása” gombig marad meg. Felvétel közben nem változtatható.

## Indítás és frissítés

- **Fej nélküli felvétel** (pl. SSH-n): `record <szcenárió> [mp]`. Enterre vagy az idő leteltével áll le, és a fenti hibatűréssel fut.
- **A programot az indítószkripttel indítjuk:** `./run.sh` (macOS/Linux) vagy `.\run.ps1` (Windows). Az argumentumokat továbbadja, pl. `./run.sh record-test 5`. Ha nincs hálózat, a frissítés nem sikerül, vagy helyi módosítás van, a meglévő verzió indul.
  - **Fejlesztő gép** (git-klón és telepített Rust): `git pull --ff-only`, fordítás, ha bármi változott, aztán indítás.
  - **Felhasználói gép** (kiadásból telepítve): ha a GitHub legújabb kiadása eltér a `VERSION` fájlban lévőtől, letölti és kicsomagolja a helyére. Nem kell Rust és fordítás. Az ffmpeg-et viszont telepíteni kell.
- **Kiadás:** a `.github/workflows/release.yml` minden `v*` címkére lefordítja a programot macOS-re (arm64 és x86_64), Linuxra (x86_64) és Windowsra (x86_64), és GitHub Release-be teszi. Lépések: a `Cargo.toml` verzióját átírni (pl. `0.2.0`), commitolni, aztán `git tag v0.2.0 && git push origin v0.2.0`. A címkének egyeznie kell a `Cargo.toml` verziójával. A repo nyilvános (2026-11-24), hogy a kiadás belépés nélkül letölthető legyen.
- **Az agentek munkakezdéskor frissítenek:** a `.claude/settings.json` SessionStart hookja minden Claude-munkamenet indulásakor lefuttatja a `.claude/hooks/git-pull.sh`-t. Helyi módosításnál nem húz le semmit, és sosem blokkolja a munkamenetet.

## Kódolási elvek

- **Hatékony kód, semmi fölösleges extra.** Csak azt építjük meg, ami kell.
- Nyelv: **Rust**, ha megoldható, vagy egy hasonlóan hatékony, natív nyelv.
- Egyszerű, átlátható felépítés. Nem kellenek felesleges absztrakciók és fancy UI.
