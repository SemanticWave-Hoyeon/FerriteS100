# FerriteS100 엔진 크레이트 분리 설계

FerriteS100을 "S-100 해도 뷰어"에서 다른 애플리케이션이 임베드할 수 있는
"S-100 해도 엔진"으로 만들기 위한 구조 분리 계획이다.

## 1. 현재 구조와 문제

- `src/` 약 3만 8천 줄 가운데 상당 부분이 엔진 기능이지만 애플리케이션 바이너리 안에 있다.
  - `main.rs`(약 1만 8천 줄): 셀 포트레이얼(`try_lua_portrayal_for_cells`), 포트레이얼 캐시
    인코딩, `prepare_instructions_*`, 레지던시·하이드레이션, 피킹(`find_features_at`, `describe_hit`).
  - 업데이트·생명주기(`s101_update_plan`, `s101_lifecycle_metadata`, `chart_publication`),
    교환 세트(`s101_xc_*`), S-102 워커(`s102_*`), S-124/S-129 오버레이(`gml_overlay`).
- `ChartApp` 구조체 하나에 해도 상태(셀, FC/PC, 커버리지, IC, 오버레이)와 UI 상태(마우스,
  줌 애니메이션, egui, LLM 채팅, 진단)가 섞여 있다.
- `ferrite-wgpu`가 렌더러(`renderer.rs` 약 1만 2천 줄)와 egui UI, winit, `ferrite-s421`에 함께
  의존한다. 렌더러만 다른 애플리케이션에서 쓸 수 없다.

## 2. 목표 구조

```
ferrite-engine   새 크레이트. winit·egui·wgpu 비의존
  ├─ catalogue   FC/PC 바인딩, 인벤토리 선택 (s101_catalogue_selection)
  ├─ datasets    탐색·열기·서명 정책·업데이트 체인·취소·캐시
  │              (dataset_discovery, dataset_open_plan, dataset_signature_policy,
  │               s101_update_plan, s101_lifecycle_metadata, chart_publication,
  │               s101_xc_*, decoded_chart_cache, secure_instruction_cache,
  │               cell_source_identity)
  ├─ products    제품 어댑터: S-101(Lua), S-102, S-124/S-129(XSLT), S-421
  ├─ portrayal   셀 포트레이얼, IC 합성, 오버레이 합성, 캐시, 레지던시
  ├─ query       피킹, 객체 정보 (dataset_pick_fields, describe_hit)
  └─ ChartEngine 공개 파사드

ferrite-gpu      ferrite-wgpu의 렌더러 부분 (egui·winit·s421 비의존)
                 입력 PreparedScene, 출력 서피스 또는 오프스크린 텍스처
ferrite-ui       egui 패널 (egui_integration, ui_chrome, overlay_ui)
ferrite-s100     애플리케이션: winit 이벤트 루프, 제스처, LLM 채팅, 플러그인, MCP, 진단·감사
```

의존 방향: `app → ui → gpu → render`, `app → engine → products/render/kernel`.
엔진은 UI 스레드를 가정하지 않는다.

## 3. 공개 API 초안

```rust
let mut engine = ChartEngine::new(EngineConfig {
    catalogues: "Catalogues".into(),
    inventory: Some(inv_dir),
    cache_dir: Some(cache_dir),
    signatures: SignaturePolicy::Optional,
})?;

let job = engine.open(DatasetSource::Folder(path)); // 백그라운드 작업 핸들
let report = job.wait()?;                           // 또는 poll() / cancel()

engine.set_display(DisplaySettings { palette: Palette::Day, viewing_time, ..Default::default() });
let scene = engine.prepare(&View { center, scale, viewport_px, pixel_ratio })?;
// PreparedScene = Arc<RenderContext> + 커버리지 + 리소스 소유자

gpu.draw(&scene, target);                           // 화면 또는 오프스크린 PNG
let hits = engine.pick(&scene, screen_pt, radius);  // Vec<PickedFeature>
let info = engine.feature(hits[0].id);              // 속성, FC 이름, 출처
```

제품 어댑터 트레이트로 S-101/S-102/S-124/S-129/S-421과 이후 S-57/S-52를 같은 방식으로 붙인다.

```rust
trait ProductAdapter {
    fn product(&self) -> ProductId;
    fn load(&self, src: &AuthorizedSource, cat: &Catalogues) -> Result<DatasetHandle>;
    fn portray(&self, ds: &DatasetHandle, ctx: &PortrayalContext) -> Result<Vec<DrawingInstruction>>;
    fn coverage(&self, ds: &DatasetHandle) -> Coverage;
    fn describe(&self, ds: &DatasetHandle, id: FeatureKey) -> FeatureReport;
}
```

## 4. 단계

각 단계의 완료 조건: S-101 렌더링이 직전 기준 커밋과 픽셀 단위로 동일
(`tools/pixel_regression.py`), 기존 테스트·`cargo clippy -- -D warnings` 통과.

| 단계 | 내용 | 위험 |
|---|---|---|
| 0 | 픽셀 회귀 하네스와 기준 위치를 `tools/`에 고정 | 낮음 |
| 1 | 순수 도메인 모듈을 `ferrite-engine`으로 이동, 애플리케이션은 import만 변경. 동작 변경 없음 | 낮음 |
| 2 | `main.rs`의 포트레이얼 파이프라인을 `engine::portrayal`로 이동, `ChartApp`의 해도 상태를 `EngineState`로 분리 | 높음 |
| 3 | `ferrite-wgpu`를 `ferrite-gpu`(렌더러, egui·winit·s421 비의존, 차트 텍스트는 epaint)와 `ferrite-ui`(egui)로 분리. 렌더러는 `SurfaceHost`·`UiLayer`·차트 텍스트 백엔드를 주입받음 | 완료 |
| 4~6 | 파사드·플랫폼 서비스(4 완료), 기능 플래그·CLI·C ABI, 웹(wasm). 4a절 참고 | 중간 |
| 7 (선택) | S-57/S-52 어댑터 | 별도 프로젝트 규모 |

## 4a. 이식성 목표 (단계 4~6으로 재편)

같은 커널을 여러 호스트에 붙인다.

```
                 ┌──────────── 호스트 ────────────┐
ferrite-engine ──┤ Rust 뷰어 (ferrite-s100, winit+egui+wgpu)
 (ECDIS 커널)    ├ CLI·라이브러리 (ferrite-cli: 렌더·검증·검사·내보내기)
                 ├ C ABI (ferrite-capi: cbindgen 헤더, C/C++/C#/Python에서 호출)
                 └ 웹 (ferrite-wasm: wasm-bindgen + WebGPU/WebGL2)
```

### 현재 이식성을 막는 것

| 의존 | 위치 | 웹(wasm) | C ABI·CLI | 대응 |
|---|---|---|---|---|
| 파일 시스템 직접 접근 (`std::fs`, `walkdir`, `tempfile`) | engine 23개 파일, s102 10, gml 6, security 4 | 불가 | 가능 | 플랫폼 서비스 트레이트 `DataSource`/`CacheStore`로 추상화. 네이티브는 파일, 웹은 메모리·IndexedDB·fetch |
| 스레드 (`std::thread`, `rayon`) | engine, render, s101 | 제한(SharedArrayBuffer 필요) | 가능 | `parallel` 기능 플래그. 끄면 순차 실행, 작업 핸들 API는 동일 |
| Lua (`mlua`, C 라이브러리) | lua-runtime → s101/s102 포트레이얼 | wasm32-unknown-unknown 불가 | 가능 | `lua-native` 기능. 웹은 (1) emscripten/WASI로 Lua C 빌드, (2) 포트레이얼 결과(Part 9a 명령) 캐시를 서버에서 미리 만들어 전달 중 선택 |
| OpenSSL (`openssl`, C) | security(X.509·ECDSA) | 불가 | 가능 | `crypto-openssl`(기본)과 순수 Rust 구현(`x509-cert`+`p384`/`p256`) 백엔드 분리. 검증 결과가 같은지 교차 테스트 |
| HDF5 (`hdf5-metno`, C) | s102 | 불가 | 가능 | `s102-hdf5` 기능. 웹은 S-102 제외 또는 순수 Rust HDF5 판독기 검토 |
| 시계·사용자 디렉터리·환경 변수 | engine(취소 이력, NAVWARN 상태, 캐시 경로) | 불가 | 가능 | `Clock`, `UserStore` 서비스로 주입 |
| GPU (`wgpu`) | ferrite-gpu | WebGPU/WebGL2 가능 | 오프스크린 가능 | 렌더러는 서피스를 주입받고, 헤드리스 오프스크린 경로 제공 |

XSLT(`ferrite-xslt`), GML(`ferrite-s100-gml`), 커널(`ferrite-kernel`), ISO 8211, 카탈로그 파서는 이미 순수 Rust라 그대로 이식된다.

### 원칙

- 커널은 경로가 아니라 바이트와 서비스 트레이트를 받는다. `EngineConfig`에 `Arc<dyn Platform>`(데이터 소스, 캐시 저장소, 사용자 저장소, 시계, 실행기)을 넣는다.
- 네이티브 전용 의존성은 모두 기능 플래그 뒤로 보낸다. 기본 기능은 지금의 네이티브 동작과 같다.
- 호스트별 바인딩은 얇게 유지한다. 커널 API(`ChartEngine`)만 감싸고 로직을 두지 않는다.
- 보안 동작은 백엔드가 바뀌어도 같아야 한다. 같은 교환 세트에 대해 두 암호 백엔드의 판정이 일치하는지 테스트한다.

### 재편한 단계

| 단계 | 내용 |
|---|---|
| 4 | **완료.** `ferrite_engine::platform`에 `Platform`(`DataSource`·`CacheStore`·`UserStore`·`Clock`·`Executor`)과 네이티브·메모리·순차 구현을 두고 `EngineState`·`GmlOverlays`에 주입했다. 카탈로그(FC 바이트, PC `CatalogueSources::from_files`), 데이터셋 탐색, S-124/S-129 카탈로그·XSLT 규칙(`ferrite_xslt::ModuleSource`)·데이터셋 읽기와 검증, 셀 포트레이얼 캐시, NAVWARN 사용자 상태, 하이드레이션 병렬 실행이 서비스를 거친다. `S101Cell::load_update_chain_bytes_with_identity`, `inspect_dataset_identification_bytes`로 파일 없이 셀을 연다. `chart_engine::ChartEngine`(`new`·`open`→`OpenJob`(`poll`·`wait`·`cancel`)·`install`·`set_display`·`prepare`→`PreparedScene`·`pick`·`feature`), `pick`(`PickSurface`, `find_features_at`, `FeatureReport`), `scene`(`PreparedScene`, 셀 리소스 바인딩, `prepare_flat_coverage`)를 추가했고 앱은 `EngineState::new`, `PreparedScene`, 엔진 피킹·`describe_feature`·리소스 바인딩을 쓴다. 예제 `crates/ferrite-engine/examples/inspect.rs`, 메모리 소스만으로 S-101·S-124를 열고 장면을 준비하는 테스트(`tests/in_memory_open.rs`). **5단계로 넘긴 네이티브 전용 경로**: S-101 서명 검증과 업데이트 계획기(`s101_update_plan`, `dataset_signature_policy`, `ferrite-security`의 경로 기반 스냅샷), 취소 이력(`chart_publication::HistoryStore`), 설치 카탈로그 인벤토리 선택(`s101_catalogue_selection`), S-102 전체(HDF5, 저널, 시험 워커), S-421 입력, S-124/S-129 교환 세트 검증·shapefile 가져오기, Lua 포트레이얼 워커 풀과 앱 로더의 스레드(`parallel` 기능으로 `Executor`에 연결 예정), PC 캡처의 rayon 병렬 읽기 |
| 5 | 기능 플래그 정리(`parallel`, `lua-native`, `crypto-openssl`/`crypto-rust`, `s102-hdf5`). 순수 Rust 암호 백엔드. `ferrite-cli`(헤드리스 렌더·검증·검사)와 `ferrite-capi`(C ABI, C 헤더, C 예제). **5B 완료**(기능 플래그·CLI·C ABI, 암호 백엔드는 5A). `ferrite-engine` 기능: `parallel`(rayon·스레드. 끄면 `default_executor()`가 `SequentialExecutor`이고 데이터 병렬 반복은 순차로 돌아 결과가 같다. 데이터 병렬은 `ferrite_kernel::par`가 rayon 또는 순차 shim을 고르며 `ferrite-render`·`ferrite-s101`·`ferrite-portrayal-catalog`의 `parallel` 기능이 켠다. Lua 포트레이얼 워커 풀과 사전 계산 페이로드 디코딩은 `Executor`로 실행), `lua-native`(mlua. `lua54`/`lua55`가 켜고 인터프리터를 고른다. 끄면 `ferrite-lua`는 Part 9a 명령 파서와 컨텍스트 모델만 빌드하고 S-101 셀은 `PortrayalSource::Precomputed`로 캐시 저장소의 셀별 사전 계산 페이로드에서 읽는다. 키는 설정·팔레트·캐시 스키마·FC/PC 다이제스트·셀 원본 SHA-256의 SHA-256이라 기계·경로·빌드가 달라도 같다. 페이로드가 없으면 명확한 오류), `s102-hdf5`(HDF5. 끄면 S-102 입력은 미지원으로 보고). 기본 기능은 셋 다 켠 기존 네이티브 동작이다. 암호 백엔드는 `crypto-openssl`(기본)과 `crypto-rust` 중 하나 이상이 필요하다. 워크스페이스의 `ferrite-security`·`ferrite-interoperability` 항목은 기본 기능을 끄고 사용 크레이트(`ferrite-engine`·`ferrite-s101`·`ferrite-s102`·`ferrite-interoperability`·`ferrite-cli`·`ferrite-capi`)가 `crypto-openssl`을 기본으로, `crypto-rust`를 선택으로 넘긴다. `cargo check -p ferrite-engine --no-default-features --features crypto-rust`는 rayon·mlua·hdf5·openssl 없이 빌드된다(`cargo tree`로 확인). `ferrite-gpu`: 서피스 없는 `GpuState::new_headless`, `WgpuRenderer::new_headless`·`render_offscreen`, `headless::{HeadlessSurface, HeadlessUi}`, epaint 차트 글꼴 `headless_text`(기존 테스트 백엔드를 승격). `ferrite-cli`: `ferrite inspect|validate|render|precompute`와 라이브러리 `OffscreenChartRenderer`(뷰어와 같은 방출: 팔레트, 표시 설정, 뷰잉 그룹, 카탈로그 소유자별 셀 리소스, 차트 텍스트). `ferrite-capi`: `include/ferrite.h`(수기, 심볼 일치 테스트), `examples/c/render.c`(테스트에서 `cc`로 빌드·실행). 파사드 추가: `DatasetSource::Bytes`, `View::camera`(`Camera`, `Zoom::Factor`/`Scale`)·`camera_view`, `exchange_set_checks`, `set_portrayal_source`·`set_precomputed_export`, 설치 카탈로그 인벤토리 소유자 선택(로컬 파일 열기), 뷰어와 같은 전체 데이터 여백(10%), `platform::DirectoryCacheStore`. 같은 위치의 CLI 렌더는 앱 스크린샷의 차트 영역과 픽셀 0.04%(SHOM `jersey_z20`), 0.26%(혼합 `portsmouth_z12`)만 다르다(가장자리 클리핑과 세계 지도 해안선). **네이티브 전용으로 남은 것**: 서명 검증·업데이트 계획기의 경로 기반 스냅샷(5A), 취소 이력, 설치 카탈로그 인벤토리(파일 시스템), S-102 전체(파사드 미지원), S-421 입력, S-124/S-129 교환 세트 검증·shapefile 가져오기, PC 캡처의 병렬 읽기(플랫폼이 없는 카탈로그 캡처라 `Executor` 대신 `parallel` 기능으로만 전환), `ferrite-gpu` 내부 rayon(네이티브 렌더러) |
| 6 | `ferrite-wasm`: wasm32 빌드(파일 없이 바이트로 열기, 순차 실행, WebGPU/WebGL2 렌더), 최소 웹 데모. Lua 포트레이얼은 미리 계산한 Part 9a 결과 또는 WASI/emscripten 빌드 중 선택 |

### 엔진 사용법

**Rust.** `ferrite-engine`(기본 기능)과 렌더링이 필요하면 `ferrite-cli` 라이브러리를 쓴다.

```rust
use ferrite_engine::chart_engine::{Camera, ChartEngine, DatasetSource, View, Zoom};
use ferrite_engine::platform::Platform;

let mut engine = ChartEngine::new(ferrite_cli::engine_config(Platform::native(), "Catalogues"))?;
let report = engine.open_blocking(DatasetSource::Folder("TestData/SHOM-S101".into()))?;
let camera = Camera { center: Some([49.21, -2.13]), zoom: Zoom::Scale(25_000.) };
let view = View::camera(1280, 800, camera);
let (_, zoom) = engine.camera_view(camera, view.viewport)?;
let mut scene = std::sync::Arc::try_unwrap(engine.prepare(&view)?).ok().unwrap();
let mut gpu = ferrite_cli::OffscreenChartRenderer::new(1280, 800, 1.0)?;
gpu.render(&engine, &mut scene, zoom)?.save("chart.png")?;
let hits = engine.pick(&scene, [640., 400.], 10., None);
```

메모리 데이터는 `Platform::in_memory`나 `DatasetSource::Bytes`로 연다. Lua 없이 빌드한 호스트는
네이티브 빌드가 `set_precomputed_export(true)`(또는 `ferrite precompute`)로 만든 페이로드를 캐시
저장소에 넣고 `set_portrayal_source(PortrayalSource::Precomputed)`로 그린다.

**CLI** (`cargo build -p ferrite-cli`, 실행 파일 `ferrite`):

```
ferrite inspect  [--json] PATH...                  # 데이터셋, 피처 유형별 개수, 검증 문제
ferrite validate [--json] [--verify] PATH...       # 검증 + 교환 세트 검사, 오류가 있으면 종료 코드 1
ferrite render PATH... --out chart.png [--center LAT,LON] [--zoom Z | --scale N]
               [--size WxH] [--density F] [--palette Day|Dusk|Night] [--precomputed DIR]
ferrite precompute PATH... --cache-dir DIR [뷰 옵션]   # Lua 없는 호스트용 셀 페이로드
```

공통 옵션은 `--catalogues DIR`(기본 `./Catalogues` 또는 `FERRITE_CATALOGUES`), `--inventory DIR`,
`-v`이다. 사용·실행 오류는 종료 코드 2.

**C** (`cargo build -p ferrite-capi` → `libferrite.{dylib,so,a}`, 헤더 `crates/ferrite-capi/include/ferrite.h`):

```c
FerriteEngine *e = ferrite_engine_create("Catalogues", NULL, NULL);
ferrite_engine_open_folder(e, "TestData/SHOM-S101");
FerriteCamera cam = {1, 49.21, -2.13, 20.0, 0.0};
ferrite_engine_prepare(e, 800, 600, 1.0, &cam);
ferrite_engine_render_rgba(e, rgba, 800 * 600 * 4);
char *hits = ferrite_engine_pick_json(e, 400, 300, 10);  /* ferrite_string_free(hits) */
ferrite_engine_destroy(e);
```

상태 코드는 `FERRITE_OK`(0) 또는 음수이고 메시지는 `ferrite_last_error()`이다. 핸들은 불투명하고
패닉은 경계를 넘지 않는다(`FERRITE_PANIC`). 예제는 `crates/ferrite-capi/examples/c/render.c`.

## 5. 주의 사항

- `main.rs`의 `audit_*` 함수가 `ChartApp` 내부를 직접 다룬다. 2단계에서 엔진 API를 거치게 하거나
  엔진 테스트 하네스로 옮긴다.
- 백그라운드 스레드는 엔진 작업 핸들(`poll`/`cancel`)로 노출한다.
- 로딩 시간·메모리 수치를 유지한다. `PreparedScene`은 `Arc`로 전달하고 복사를 추가하지 않는다.
- 캐시 직렬화 형식이 바뀌면 `CACHE_SCHEMA_VERSION`을 올리고 이전 스키마 거부 테스트를 유지한다.

## 6. 픽셀 회귀 하네스 (0단계)

`tools/pixel_regression.py`가 기준 리비전과 작업 트리를 각각 빌드해 같은 데이터·위치·팔레트로
화면을 렌더링하고 RGBA 버퍼를 비교한다. 위치와 데이터는 `tools/pixel_regression_poses.json`에 둔다.

```
python3 tools/pixel_regression.py                 # HEAD 대 작업 트리
python3 tools/pixel_regression.py --base 3a72f5d  # 특정 커밋 대 작업 트리
python3 tools/pixel_regression.py --skip-build    # 이전 빌드 재사용
```

- 빌드 결과와 스크린샷은 저장소 밖(기본: 시스템 임시 폴더의 `ferrite-pixel-regression`)에 둔다.
- Night 팔레트는 카탈로그 사본의 Day/Night 팔레트 이름을 맞바꿔 렌더링한다.
- 셀 포트레이얼 캐시는 각 실행 파일 옆 `cache` 폴더를 쓰며, 위치마다 캐시를 비운 실행과 캐시를
  채운 뒤의 재실행을 모두 비교한다.
- 비교기는 같은 화면이면 동일, Day와 Night는 다르게 판정되는지를 매번 자체 점검한다.
