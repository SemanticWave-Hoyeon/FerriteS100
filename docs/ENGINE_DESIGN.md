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
| 4~6 | 파사드·플랫폼 서비스, 기능 플래그·CLI·C ABI, 웹(wasm). 모두 완료, 4a절 참고 | 중간 |
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
| Lua (`mlua`, C 라이브러리) | lua-runtime → s101/s102 포트레이얼 | 가능(`lua-wasm`, 6b단계) | 가능 | `lua-native` 기능. 웹은 같은 Lua 5.4 C 소스를 clang wasm32로 빌드(`ferrite-lua-runtime`, 최소 libc와 wasm 예외 처리 setjmp/longjmp). 사전 계산 페이로드도 계속 지원 |
| OpenSSL (`openssl`, C) | security(X.509·ECDSA) | 불가 | 가능 | `crypto-openssl`(기본)과 순수 Rust 구현(`x509-cert`+`p384`/`p256`) 백엔드 분리. 검증 결과가 같은지 교차 테스트 |
| HDF5 (`hdf5-metno`, C) | s102 | 가능(순수 Rust 판독기) | 가능 | `ferrite-s102`의 `h5` 계층 뒤에 C 라이브러리(`hdf5` 기능, 로컬 경로 기본)와 순수 Rust 판독기(`h5::pure`, 항상 빌드)를 둠. 엔진 `s102-pure`(C 없음, wasm)·`s102-hdf5`. 6c절 |
| 시계·사용자 디렉터리·환경 변수 | engine(취소 이력, NAVWARN 상태, 캐시 경로) | 불가 | 가능 | `Clock`, `UserStore` 서비스로 주입(취소 이력은 6a에서 `UserRecord`로 전환) |
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
| 5 | 기능 플래그 정리(`parallel`, `lua-native`, `crypto-openssl`/`crypto-rust`, `s102-hdf5`). 순수 Rust 암호 백엔드. `ferrite-cli`(헤드리스 렌더·검증·검사)와 `ferrite-capi`(C ABI, C 헤더, C 예제). **5B 완료**(기능 플래그·CLI·C ABI, 암호 백엔드는 5A). `ferrite-engine` 기능: `parallel`(rayon·스레드. 끄면 `default_executor()`가 `SequentialExecutor`이고 데이터 병렬 반복은 순차로 돌아 결과가 같다. 데이터 병렬은 `ferrite_kernel::par`가 rayon 또는 순차 shim을 고르며 `ferrite-render`·`ferrite-s101`·`ferrite-portrayal-catalog`의 `parallel` 기능이 켠다. Lua 포트레이얼 워커 풀과 사전 계산 페이로드 디코딩은 `Executor`로 실행), `lua-native`(mlua. `lua54`/`lua55`가 켜고 인터프리터를 고른다. 끄면 `ferrite-lua`는 Part 9a 명령 파서와 컨텍스트 모델만 빌드하고 S-101 셀은 `PortrayalSource::Precomputed`로 캐시 저장소의 셀별 사전 계산 페이로드에서 읽는다. 키는 설정·팔레트·캐시 스키마·FC/PC 다이제스트·셀 원본 SHA-256의 SHA-256이라 기계·경로·빌드가 달라도 같다. 페이로드가 없으면 명확한 오류), `s102-hdf5`(HDF5. 끄면 S-102 입력은 미지원으로 보고). 기본 기능은 셋 다 켠 기존 네이티브 동작이다. 암호 백엔드는 `crypto-openssl`(기본)과 `crypto-rust` 중 하나 이상이 필요하다. 워크스페이스의 `ferrite-security`·`ferrite-interoperability` 항목은 기본 기능을 끄고 사용 크레이트(`ferrite-engine`·`ferrite-s101`·`ferrite-s102`·`ferrite-interoperability`·`ferrite-cli`·`ferrite-capi`)가 `crypto-openssl`을 기본으로, `crypto-rust`를 선택으로 넘긴다. `cargo check -p ferrite-engine --no-default-features --features crypto-rust`는 rayon·mlua·hdf5·openssl 없이 빌드된다(`cargo tree`로 확인). `ferrite-gpu`: 서피스 없는 `GpuState::new_headless`, `WgpuRenderer::new_headless`·`render_offscreen`, `headless::{HeadlessSurface, HeadlessUi}`, epaint 차트 글꼴 `headless_text`(기존 테스트 백엔드를 승격). `ferrite-cli`: `ferrite inspect|validate|render|precompute`와 라이브러리 `OffscreenChartRenderer`(뷰어와 같은 방출: 팔레트, 표시 설정, 뷰잉 그룹, 카탈로그 소유자별 셀 리소스, 차트 텍스트). `ferrite-capi`: `include/ferrite.h`(수기, 심볼 일치 테스트), `examples/c/render.c`(테스트에서 `cc`로 빌드·실행). 파사드 추가: `DatasetSource::Bytes`, `View::camera`(`Camera`, `Zoom::Factor`/`Scale`)·`camera_view`, `exchange_set_checks`, `set_portrayal_source`·`set_precomputed_export`, 설치 카탈로그 인벤토리 소유자 선택(로컬 파일 열기), 뷰어와 같은 전체 데이터 여백(10%), `platform::DirectoryCacheStore`. 같은 위치의 CLI 렌더는 앱 스크린샷의 차트 영역과 픽셀 0.04%(SHOM `jersey_z20`), 0.26%(혼합 `portsmouth_z12`)만 다르다(가장자리 클리핑과 세계 지도 해안선). **네이티브 전용으로 남은 것**: 서명 검증·업데이트 계획기의 경로 기반 스냅샷(5A), 취소 이력, 설치 카탈로그 인벤토리(파일 시스템), S-102 전체(파사드 미지원), S-421 입력, S-124/S-129 교환 세트 검증·shapefile 가져오기, PC 캡처의 병렬 읽기(플랫폼이 없는 카탈로그 캡처라 `Executor` 대신 `parallel` 기능으로만 전환), `ferrite-gpu` 내부 rayon(네이티브 렌더러). 서명 검증·업데이트 계획기·취소 이력·설치 카탈로그 인벤토리·S-421 입력은 6a에서 플랫폼 서비스로 옮겼다 |
| 6 | **완료.** `ferrite-wasm`(wasm-bindgen, `crates/ferrite-wasm`)과 웹 데모(`crates/ferrite-wasm/www`, 빌드 `tools/build_wasm.py`). `cargo check -p ferrite-engine --target wasm32-unknown-unknown --no-default-features --features crypto-rust`가 통과하고 `ferrite-gpu`·`ferrite-cli` 라이브러리도 wasm32로 빌드된다. **wasm 차단 요소와 대응**(네이티브 동작은 그대로): `getrandom` 0.3(regexml→ahash)은 wasm32 대상에서만 `wasm_js` 기능(`ferrite-portrayal-catalog`), `std::time::Instant::now`(wasm에서 패닉)는 `ferrite_kernel::time::Instant`(네이티브는 `std::time::Instant` 그대로, wasm은 `web-time` =1.1.0)로 엔진·렌더·GPU의 타이밍 로그와 NAVWARN 재검사 시각을 바꿈, `SystemClock`은 wasm에서 chrono(JS `Date`)로 시각을 읽고 웹 바인딩은 `Date.now()` 기반 `JsClock`을 주입, `ferrite-s421`(플러그인 계층 `abi_stable`/`libloading`이 wasm 불가)은 엔진에서 wasm이 아닌 대상에만 의존하고 웹은 `s421_dataset_input`의 `Profile`·`MAX_XML_BYTES` 사본을 씀(일치 테스트 포함), `wgpu`는 wasm 대상에서 `webgl`·`fragile-send-sync-non-atomic-wasm` 기능, `GpuState`는 웹에서 WebGPU 감지 후 없으면 WebGL2(다운레벨 한도)로 인스턴스를 만듦, wasm의 `std::fs::File`에는 `Drop`이 없어 네이티브 전용 원자 쓰기 경로 세 곳의 `drop(file)`에 `clippy::drop_non_drop` 허용. rayon(`ferrite-gpu` 직접 의존과 `ferrite-render/parallel`)은 wasm에서 rayon-core가 호출 스레드 하나로 대체한다. **API** `WebEngine`: `new([[이름, 바이트]...])`/`from_bundle(catalogues.bin)`(FC/PC S-101 필수, S-124·S-129 선택), `add_file(path, bytes)`, `open()`→보고서 JSON, `report()`, `palettes()`, `set_palette`, `load_precomputed(name, bytes)`, `prepare(w, h, density, lat, lon, scale_or_zoom, is_scale)`→장면 요약 JSON, `pick_json(x, y, r)`, `feature_json(cell_index, feature_id)`, `render_to_canvas(WebCanvas)`. `WebCanvas.create(canvas_id, density, backend?)`(async)는 캔버스 서피스에 `ferrite-gpu`를 붙이고 `adapter()`로 어댑터와 백엔드를 알려 준다. `backend`는 `"auto"`(기본: WebGPU가 있으면 WebGPU, 없으면 WebGL2), `"webgpu"`, `"webgl2"`이고 `WgpuRenderer::new_with_backends`/`GpuState::new_with_backends`로 wgpu 인스턴스 백엔드(`BROWSER_WEBGPU`, `GL`)를 제한한다(네이티브 `new`는 그대로 `Backends::all()`). 엔진은 `Platform::in_memory` + `SequentialExecutor` + `MemoryCacheStore`이고 S-101은 항상 `PortrayalSource::Precomputed`이다. 렌더러 방출은 CLI와 같은 `ferrite_cli::SceneEmitter`(이번에 `OffscreenChartRenderer`에서 분리)이다. 내장 브라우저(WebGPU, Apple M5)에서 SHOM 교환 세트(셀 4개, 사전 계산 페이로드)·UKHO S-124 5개·IHO S-129를 열어 같은 위치의 CLI 렌더와 같은 명령 수(70114)·모습으로 그리고, 팬·줌·피킹·Night 팔레트를 확인했다. **WebGL2**: 데모의 `?backend=webgl2`(또는 백엔드 선택)로 같은 브라우저에서 WebGL2(ANGLE Metal)를 강제해 같은 샘플을 그려 보니 수정 없이 동작했다. 명령 수(70114)·축척이 WebGPU와 같고, Day 개요·줌 2.7·줌 20과 Night 줌 2.7 스크린숏이 WebGPU와 픽셀 단위로 사실상 같았으며(JPEG 잡음 수준), 팬·줌·피킹·리사이즈·팔레트 전환에서 콘솔 오류가 없었다. 렌더러의 기본 경로는 저장 버퍼·컴퓨트를 쓰지 않는다(유일한 컴퓨트 경로 `retained_world_area`는 `FERRITE_RETAINED_WORLD_AREAS` 옵트인이고, 켜더라도 다운레벨 한도의 저장 버퍼 크기 0에서 기존 한도 검사로 사양된다). 타임스탬프 쿼리는 어댑터가 제공할 때만 요청한다. **남은 것**: S-102(6c·6d), 웹 워커·OffscreenCanvas, IndexedDB 캐시(6d). Lua 포트레이얼과 증분 렌더는 6b단계 |
| 6a | **완료. 남은 네이티브 전용 경로의 플랫폼 서비스 전환.** 네이티브 동작(같은 파일·순서·스냅숏·오류)은 그대로이고 경로 함수는 `NativeDataSource`를 쓰는 `_in` 함수의 래퍼다. **S-101 서명 검증·업데이트 계획**: `s101_update_plan`의 `expand_candidates_in`·`authorized_folder_plans_in`·`authorized_batch_in`·`authorized_plans_in`(test-support)과 새 `authorized_selection_plans_in`이 `DataSource`로 읽는다. 로컬 소스는 예전처럼 비공개 스냅숏 파일(인증 스냅숏 파일, `UnauthenticatedSnapshot::copy_bounded`)을, 그 밖의 소스는 소유 바이트(인증 스냅숏의 메모리 바이트, 같은 상한의 사본)를 보관하고 `Plan::load`는 `S101Cell::load_update_chain_bytes_with_identity`로 같은 한도·검증·원본 식별자를 낸다. `s101_lifecycle_metadata::capture_cached_in`과 `MetadataEvidence::verify_retained`(`RetainedInput::{File, Bytes}`)는 비검증 `CATALOG.XML`도 소스로 읽고 `dataset_signature_policy::unchecked_datasets_in`은 소스의 정규 경로를 키로 쓴다. `s101_xc_*`·`cell_source_identity`·`chart_publication`의 생명주기 검사는 원래부터 보관된 바이트만 다룬다. `ChartEngine`은 로컬 소스이거나 `SignaturePolicy::Required`이면 이 계획기로 열고(메모리 경로는 권한 키이므로 절대 경로), `Off`인 비로컬 소스는 기존 바이트 계획을 유지한다(웹의 상대 경로 이름). 폴더에서 찾은 S-101 입력은 정규 경로로 바꿔 계획한다(이전에는 `..`가 든 폴더 경로에서 생명주기 메타데이터 키가 어긋나 셀이 거부됐다). **취소 이력**: `UserStore::cancellation_history`(`UserRecord`, 새 `UserRecord::lock`)로 옮겼다. 네이티브는 `chart_publication::HistoryFile`(같은 파일, `.lock` OS 잠금, `create_new` 임시 파일과 rename), `MemoryUserStore`는 `s101-cancellations` 레코드(`MEMORY_HISTORY_KEY`)다. `HistoryStore::begin_record`·`CancellationHistory::read_record`를 추가했고 앱이 쓰는 경로 함수(`begin`, `read`)는 그대로다. 파사드의 `DatasetSource::Files`/`Bytes`는 명시 선택이므로 뷰어처럼 선택한 갱신의 기본·선행 파일을 함께 열고(`expand_candidates_in`), 선택된 물리 취소를 로드된 대상(또는 대상이 없으면 공지)과 이력으로 검증해 적용한 뒤 이력에 기록한다(`OpenReport::cancellations`, `ChartEngine::cancellation_history`). 폴더에서 발견한 취소는 적용하지 않고, 설치되는 데이터셋은 이력의 이름 재사용 규칙(`validate_reuse`)을 지켜야 한다. **설치 카탈로그 인벤토리**: `resolve_pair_exact_first_in`·`DatasetCatalogueRegistry::stage_in`, `EngineState`와 파사드는 플랫폼 소스로 인벤토리를 읽는다. **S-421 입력**: `s421_dataset_input::capture_if_route_in`·`discover_routes_in`. 파사드는 S-421 경로를 캡처해 `ChartEngine::routes()`로 내준다(포트레이얼 없음, `Required`에서는 맨 GML 경로를 받지 않음). 테스트 `tests/in_memory_exchange_sets.rs`: SHOM 2026년 5월 교환 세트 전체와 ed1er2(기본+갱신 2개)를 메모리와 네이티브에서 `Required`(고정 시계)로 열어 데이터셋·판·갱신·원본 식별자·교환 세트 검사가 같은지, 변조되거나 빠진 갱신의 거부, 서명된 합성 취소(독립 설치 루트)의 폴더 비적용·명시 적용·`MemoryUserStore` 이력·이름 재사용 거부·다른 엔진에서의 재생, 메모리 S-421 경로. **네이티브 전용으로 남은 것**: S-102 전체(별도 작업), `ferrite-s421`의 플러그인 계층과 경로 포트레이얼(앱의 `native_s421`), 앱의 열기 대화 계획(`dataset_open_plan`), S-124/S-129 shapefile 가져오기, `ferrite_security::capture_catalogue_authenticated_incoming_exchange`(수신 폴더 캡처), 파일 기반 캐시 구현(`NativeCacheStore`·`secure_instruction_cache`), PC 캡처의 병렬 읽기, `ferrite-gpu` 내부 rayon |
| 6b | **완료. 웹 Lua 포트레이얼과 증분 렌더.** **Lua 방식 선택**: (a) Lua C 소스를 wasm32-unknown-unknown으로 컴파일, (b) 순수 Rust Lua 구현, (c) 웹 빌드 전체를 wasm32-unknown-emscripten으로 전환을 검토해 (a)를 택했다. (c)는 wasm-bindgen이 emscripten 대상을 지원하지 않아 웹 바인딩과 wgpu 경로를 새로 짜야 하고, (b)는 S-101 규칙 전체(문자열 서식, 패턴, 코루틴, pcall)를 C Lua와 바이트 단위로 같게 돌린다고 볼 만큼 성숙한 크레이트가 없다. (a)는 WebAssembly 대상이 있는 clang(Homebrew LLVM 23에서 확인)과 `llvm-ar`만 있으면 되고 네이티브와 같은 인터프리터 소스를 쓴다. **구현**: `ferrite-lua-runtime`의 mlua 의존을 대상별로 나눴다. 네이티브는 그대로 `vendored`(lua-src 550.0.0, Lua 5.4.8)이고, wasm32-unknown-unknown은 mlua-sys `external`(=0.10.0)이며 빌드 스크립트가 같은 lua-src 소스를 lua-src의 WASI 설정(`LUA_USE_POSIX`+WASI 가드, `LUA_COMPAT_5_3`, `-mllvm -wasm-enable-sjlj` 새 예외 처리(exnref))으로 컴파일해 링크한다(`FERRITE_WASM_CC`·`FERRITE_WASM_AR`로 도구 지정, 없으면 Homebrew LLVM 경로 또는 PATH). 시스템 헤더 대신 `wasm-libc/include`의 최소 헤더를 쓰고, `wasm-libc/ferrite_libc.c`가 문자열·ctype·`vsnprintf`·스트림(파일 없음, stdout/stderr 버림)·`__wasm_setjmp`/`__wasm_longjmp`와 `__c_longjmp` 태그·`frexp`를, `src/wasm_libc.rs`가 힙(`malloc` 등)과 부동소수 변환(`%e %f %g %a`, `strtod`)을 Rust의 정확 반올림 서식·파서로 제공한다. 수학 함수와 `mem*`는 Rust compiler-builtins를 쓴다. 서식·파서는 네이티브에서 호스트 C 라이브러리와 비교하는 테스트(값 약 1만 2천 개 × 서식 20개, 파싱 입력 약 1만 6천 개)로 확인했다. Apple libc와 다른 두 경우(`%g`가 정확한 십진 동률을 0으로 내림 반올림할 때 끝 0을 남김, `%.Na` 절삭)는 C99·glibc 동작을 따른다. `table.sort` 피벗 난수(`arc4random`)는 고정값이다. wasm-bindgen 0.2.108(walrus 0.24)이 예외 처리 명령을 그대로 통과시키고, Lua `error`/`pcall`은 Rust 프레임을 넘는 wasm 예외로 동작한다. `ferrite-lua`의 시간 측정은 `ferrite_kernel::time::Instant`로 바꿨다(wasm의 `std::time::Instant`는 패닉). 웹 기능은 `ferrite-wasm/lua-wasm`(엔진 `lua54`)이고 `tools/build_wasm.py`는 기본으로 켠다(`--no-lua`로 끔). 네이티브 기본 빌드는 바뀌지 않는다. **동일성**: `tools/lua_wasm_parity.py`가 같은 `WebEngineCore`로 네이티브(`examples/part9a_dump.rs`, mlua)와 wasm(Node, `tools/lua_wasm_parity.mjs`)에서 데이터셋 묶음마다 `HostPortrayalEmit` 호출(피처 참조, Part 9a 그리기 명령 문자열, 관찰 문맥 매개변수)을 순서대로 기록해 비교한다(`PortrayalEngine::process_owned_cell_recording`). TestData의 SHOM·UKHO·IHO S-101 58개 묶음(내용이 같은 묶음은 한 번, 셀 61개, Part 9a 출력 91,954개)이 모두 바이트 단위로 같고, 네이티브를 두 번 돌린 결과도 같다. 관찰 매개변수 목록은 규칙이 Lua 테이블 순회 순서로 만들기 때문에 해시 시드(네이티브도 주소·시각 기반)에 따라 순서가 달라지므로 정렬해 비교한다(엔진도 정렬해 쓴다). 그리기 명령 문자열은 정렬 없이 같다. UKHO S-101 시험 셀 17개와 IHO 시험 셀 9개는 사양 1판이라 두 빌드 모두 같은 오류로 열지 않는다. **포트레이얼 선택**: `WebEngine.set_portrayal("auto"|"lua"|"precomputed")`, `portrayal()`, `WebEngine.lua_available()`. `auto`는 Lua가 빌드되어 있고 사전 계산 페이로드를 넣지 않았으면 Lua를 쓰고, 페이로드를 넣었으면 페이로드를 쓰되 빠진 셀이 있으면 그 준비를 Lua로 다시 한다. 장면 요약의 `portrayal`과 데모 HUD·S-101 선택 상자가 사용한 소스를 보여 준다. 웹 Lua도 메모리 캐시 저장소의 셀 명령 캐시를 써서 첫 준비 뒤의 재포트레이얼은 캐시를 읽는다. **증분 렌더**: `ChartEngine::reproject(scene, view)`가 뷰어의 팬·줌 경로(`rebuild_view_ex`: 카메라·뷰포트·픽셀 비율, 새 뷰의 평면 커버리지)만 수행하고, 새 뷰의 레지던시(`residency_for_view`)가 장면의 레지던트 셀과 다르면 false를 돌려 `prepare`가 다시 포트레이얼하게 한다. `WebEngine.set_view(...)`(인자는 `prepare`와 같음)는 데이터셋·표시·포트레이얼 선택이 그대로면 재투영하고 아니면 준비하며, 요약의 `reprojected`로 알려 준다. 재투영한 장면은 같은 뷰를 새로 준비한 장면과 명령 수·축척이 같고 오프스크린 렌더 픽셀이 같다(`ferrite-wasm` 테스트, S-124와 Lua S-101). GPU 쪽은 기존 방출 경로(`SceneEmitter`)를 그대로 쓴다. **브라우저 측정**(내장 브라우저, Apple M5, 1920×1600 물리 픽셀, 사전 계산 페이로드 없음): SHOM 2026년 3월 교환 세트(셀 4개, 69,570 명령)는 Lua 첫 준비 2.4 s(WebGL2 4.5 s), 팬·줌 한 번에 전체 `prepare` 200~220 ms(셀 캐시 적중)였던 것이 `set_view` 2~6 ms, 그리기 12~20 ms(첫 프레임 뒤)가 됐다. UKHO S-101 교환 세트(셀 27개, 247,414 명령)는 Lua 첫 준비 7.2 s, 전체 `prepare` 1,054 ms → `set_view` 10~20 ms(한 번 132 ms), 그리기 37~570 ms. 같은 위치의 CLI 렌더와 명령 수·모습을 비교했다(아래). **남은 것**: 웹 워커·OffscreenCanvas와 IndexedDB 캐시는 6d에서 완료. GPU 쪽 증분(재투영 뒤에도 모든 명령을 다시 방출하므로 큰 장면은 그리기가 수백 ms), 셀 단위 사전 계산·Lua 혼합(`auto`는 준비 단위로 전환), Lua 5.5 wasm 빌드 검증, 레지던시가 켜지는 큰 셀 묶음의 브라우저 측정(시험 데이터는 임계값 아래) |
| 6c | **완료. S-102를 바이트와 순수 Rust HDF5로.** **판독기 설계**: `ferrite-s102::h5`가 S-102 어댑터가 쓰는 HDF5 부분(그룹·링크·속성·데이터셋, `hdf5` 크레이트와 같은 이름의 `TypeDescriptor`, `read_raw`·`read_slice_2d`·이름 기준 복합 멤버 읽기 `read_field`·`read_f32_fields_2d`, `H5Dvlen_get_buf_size`와 같은 가변 길이 예측)을 백엔드 중립 API로 감싼다. 백엔드는 `hdf5-metno`(새 `hdf5` 기능, 기본, 로컬 경로의 기본 판독기)와 `h5::pure`(외부 크레이트 없이 새로 작성, 항상 빌드)이다. 검토 시점에 성숙하고 고정 가능한 순수 Rust HDF5 판독 크레이트가 없어 S-102에 필요한 범위만 구현했다: 수퍼블록 v0~v3, 객체 헤더 v1/v2(연속 블록, lookup3 체크섬 검증), 옛 그룹(심볼 테이블, v1 B-트리, 로컬 힙)과 새 그룹(링크 메시지, 프랙털 힙+v2 B-트리 밀집 링크), 압축·밀집 속성, 정수·IEEE 실수·고정/가변 문자열·복합·열거·배열 형식(빅엔디언 포함), 압축·연속·청크 레이아웃(v1 B-트리, 단일 청크, 암시적, 고정 배열, v2 B-트리 색인), deflate(flate2 =1.1.8)·shuffle·Fletcher-32 필터, 채움 값. 공유 메시지·커밋된 형식·외부/가상 저장소·szip·n-bit·scale-offset·확장 배열 색인·거대 힙 객체·필터 힙·소프트/외부 링크 대상은 오류로 거부한다(테스트 포함). 형식 변환은 C 라이브러리를 따른다(정수 범위 클리핑, 열거→정수, 고정 문자열 패딩 규칙 `H5T__conv_s_s`, 가변 문자열의 C 문자열 의미, `H5T__set_loc`의 메모리 배치 기술자). 경로용 판독기는 `FERRITE_S102_HDF5_READER=pure`로 바꿀 수 있다. 기존 크레이트 API(`singleton`, `pub use hdf5`)는 엔진의 `dataset_open_plan`을 위해 남겼다. **동일성**: `tests/reader_parity.rs`가 TestData의 S-102 파일 89개(내용이 다른 50개: UKHO 시험 셀 2.1, UKHO 2026 3.0, SHOM 3.0, 모두 수퍼블록 v0·청크 deflate)와 C 라이브러리로 만든 합성 파일(기본·earliest·latest 형식, 청크·deflate·shuffle·Fletcher-32, 밀집 속성·링크, 고정 배열·v2 B-트리·단일 청크 색인, 채움 값, 열거·불리언·고정/가변 문자열·빅엔디언)을 두 판독기로 읽어 구조·속성 값·데이터셋 값·가변 길이 예측과 S-102 해석(메타데이터, 전체 격자 샘플 비트, 품질 레코드·ID, 시험 셀 창)을 비교한다. 모두 같다. 메모리 이미지도 파일과 같다. 크레이트 단위 테스트(138개)는 `FERRITE_S102_HDF5_READER=pure`에서도 모두 통과한다. 2501×2502 격자(UKHO 2026, 6.3백만 샘플) 전체 창 읽기는 release에서 C 약 0.2 s, 순수 Rust 약 0.33 s(deflate가 miniz_oxide)이다. **엔진**: 기능 `s102-pure`(C 없음, `cargo check -p ferrite-engine --target wasm32-unknown-unknown --no-default-features --features crypto-rust,s102-pure` 통과)와 `s102-hdf5`(= `s102-pure` + C 판독기 + Lua). `ferrite-s102`는 `lua-native` 없이도 빌드되며 그때 포트레이얼은 규칙이 낸 Part 9a 명령 문자열(`BathymetryPortrayal::rule_instructions`/`from_bound_instructions`)로 만든다. `s102_input_capture::CapturedInput::capture_in`(데이터 소스 또는 인증 메모리 스냅숏의 바이트, 같은 512 MiB 상한, `verify`는 바이트 해시), `s102_depth_policy::DepthPolicy::from_bytes`, `s102_trial_worker::inspect_bytes`(프로세스 격리 없이 순수 판독기), `s102_journal_admission`의 메모리 스냅숏 검증. 파일 경로 함수와 앱 동작은 그대로다. **파사드**: `ChartEngine::new`가 `PC/S-102`를 데이터 소스로 읽고(선택), `open`은 `.h5`/`.hdf5` 입력을 서명 정책(`authorize`)과 캡처를 거쳐 메모리 바이트를 순수 판독기로 해석한다(`productSpecification` 3.0.0은 커버리지, 2.1은 검사 전용 시험 셀, 그 밖은 문제). `install`은 뷰어처럼 `EngineState::bathymetry`·`depth_policies`·`bathymetry_bounds`·`s102_trials`에 넣고 `OpenReport::bathymetry`(`BathymetrySummary`)로 알린다. S-102 PC가 없으면 커버리지는 문제로 거부한다. `prepare`는 `PreparedScene::bathymetry`(`BathymetryScene`: 표시 모드의 뷰잉 그룹, 파일마다 원본 또는 보수적 데이텀 모자이크를 `tile_edge`(기본 2048) 창으로 나눈 `BathymetryTile::Bound`(PC 평가 봉인 재질) 또는 IC 합성 `Composed`)을 만든다. 데이터 범위에 래스터 범위를 더한다. Lua가 없으면 캐시 저장소의 사전 계산 규칙 명령(`set_precomputed_export`로 Lua 빌드가 기록, SHA-256 무결성)을 쓰고 없으면 명확한 오류다. API: `ChartEngine::bathymetry()`, `s102_options()`/`set_s102_options(S102Options { depth_adjustments, tile_edge })`, `s102_scene::SUPPORT`. 테스트 `tests/in_memory_s102.rs`: 메모리 소스의 SHOM 3.0 두 개와 UKHO 2.1 시험 셀을 열고 준비, Lua 타일과 사전 계산 타일의 픽셀 동일, 앱 파이프라인(C 판독기, 파일 깊이 정책, Lua 규칙)과 타일 픽셀 동일, PC 없음·손상 바이트의 문제 보고. **남은 것**: 장면의 S-102 타일을 GPU로 그리는 호스트 방출은 6d에서 완료. EPSG:4326 외 좌표계와 다각형 도메인의 포트레이얼(뷰어와 같이 거부), 품질 속성 해석의 프로세스 격리(파사드는 프로세스 안에서 해석), 파사드의 S-102 취소 저널 게이트(앱만 사용), 확장 배열 청크 색인 등 위 거부 목록 |
| 6d | **완료. 호스트의 S-102 그리기, 웹 워커·OffscreenCanvas, IndexedDB 포트레이얼 캐시.** **S-102 방출**: `ferrite_cli::SceneEmitter`(CLI 오프스크린 렌더러, C ABI, 웹 캔버스 공용)가 `PreparedScene::bathymetry`의 타일을 뷰어와 같은 래스터 장면 게시로 GPU에 올린다. `BathymetryTile::Bound`는 `S102RasterMaterial::Bound`(PC 평가 봉인 유지), `Composed`는 `LegacyIc`로 `stage_s102_raster_batch` → `prepare_raster_material_publication(교체)` → 표시 모드 뷰잉 그룹과 `prepare_raster_scene_publication_with_groups` → 검증 → `commit_raster_scene_publication` 순서이고, 뷰어처럼 `begin_frame` 뒤, 벡터 방출 앞에서 커밋한다. 상호운용 결합이 S-102를 억제하면 뷰어의 `update_raster_suppression`과 같은 조건으로 래스터 억제를 켠다. 타일은 업로드 때 소비되어 CPU 픽셀이 풀리므로(뷰어와 같음) `BathymetryScene::id`(준비마다 고유)로 같은 장면의 다음 프레임(재투영)은 보관한 래스터를 `update_raster_view`로 재투영만 하고, S-102가 없는 장면은 래스터를 지우며, 타일을 다른 렌더러가 가져간 장면은 조용히 빠뜨리지 않고 오류로 거부한다. `ferrite-gpu`의 `s102-portrayal`은 `s102-material`(형식이 있는 S-102 재질, 암호 백엔드는 의존 크레이트가 고름)과 OpenSSL로 나눴다(뷰어의 선택은 그대로). 기능: `ferrite-cli`·`ferrite-capi`의 `s102-pure`(엔진 `s102-pure` + `ferrite-gpu/s102-material`)와 `s102-hdf5`(= `s102-pure` + C 판독기·Lua, 기본), `ferrite-wasm`의 `s102-pure`(기본). `ferrite inspect --json`은 `bathymetry`(파일별 사양·격자·데이텀·범위)를 낸다. **확인**: SHOM S-101 교환 세트 + SHOM S-102 7개(생말로)를 같은 위치에서 앱(`--chart`·`--s102`·`--screenshot`, `FERRITE_BACKGROUND_TEST=1`)과 `ferrite render`(앱 차트 영역 1420×862, `--density 2`)로 그려 비교했다. 줌 10: 1,224,040픽셀 중 372개(0.030%, 채널 차 32 초과 0.0024%), 줌 60: 706개(0.058%, 32 초과 0.024%)만 다르다(S-101만 그린 같은 화면과의 차이 0.036%와 같은 수준의 글자·가장자리 잡음). S-102를 뺀 CLI 렌더와는 1.74% 다르다. 테스트: `ferrite-cli` `renders_s102_bathymetry`(검사 JSON, S-102 유무 차이, 사전 계산 S-101 페이로드·S-102 규칙 명령으로 같은 이미지), `ferrite-wasm` `s102_from_bytes_is_drawn`(메모리 바이트, Lua 규칙, 같은 렌더러의 재투영 프레임이 새 준비와 같음, 다른 렌더러 거부). **웹 워커**: 데모의 엔진은 모듈 워커(`www/worker.js`)에서 돌고 페이지 캔버스를 `transferControlToOffscreen()`으로 넘겨 그린다. 새 API `WebCanvas.from_offscreen(OffscreenCanvas, density, backend)`(wgpu `SurfaceTarget::OffscreenCanvas`, 크기는 `resize`가 오프스크린 캔버스에 설정). 엔진 쪽 동작은 `www/host.js`(`EngineHost`: 시작, 파일 추가·열기, 렌더, 피킹, 캐시) 하나이고, 워커는 요청을 도착 순서대로 하나씩 실행한다. `www/engine-client.js`의 `connect`가 실행 위치를 고른다: OffscreenCanvas·모듈 워커가 있으면 워커, 페이지에는 WebGPU가 있는데 워커에 없으면(`auto`) 또는 워커가 GPU 컨텍스트를 만들지 못하면 메인 스레드(같은 `EngineHost`, 캔버스 요소; 넘겨진 캔버스는 새 요소로 교체). `?worker=auto|on|off`, 화면의 Engine 선택. 기존 `WebEngine`/`WebCanvas.create` API는 그대로이고 메인 스레드 경로가 그대로 쓴다. 페이지는 프레임 요청을 하나씩 보내고 그 사이 요청은 하나로 합치며, 늦게 끝난 프레임이 새 카메라를 덮어쓰지 않는다. **IndexedDB 캐시**: 엔진의 `CacheStore`는 동기이고 IndexedDB는 비동기라 경계를 엔진 호출 사이에 둔다. `ferrite-wasm`의 `PageCacheStore`(메모리 저장소 + 엔진이 쓴 항목 기록 + 읽기 수)와 `ChartEngine::cell_cache_entries()`(현재 표시로 다음 `prepare`가 읽을 셀별 Lua 명령 캐시 항목; 키는 `prepare`가 먼저 고르는 색상 프로파일로 계산)를 두고, `WebEngine.missing_cache_entries()`(메모리에 없는 항목 이름), `load_cache_entry(name, bytes)`(기록하지 않는 사전 적재), `take_cache_writes()`(새로 쓴 `[이름, 바이트]`), `scene_current()`(재투영 가능 여부)를 추가했다. 호스트는 재투영이 아닐 때 준비 전에 빠진 항목을 IndexedDB(`www/cache-db.js`, 저장소 `ferrite-s100-portrayal-cache`, 항목별 사용 시각, 512 MiB 넘으면 오래된 것부터 삭제)에서 읽어 넣고, 준비 뒤 엔진이 쓴 항목을 저장한다. 항목은 엔진의 스키마·무결성 검사를 그대로 거치므로 낡은 항목은 거부되어 다시 포트레이얼된다. 사전 계산 페이로드는 저장 대상이 아니다. 장면 요약에 `cache_reads`(이번 준비의 캐시 적중)와 `bathymetry_files`를 더했다. 테스트 `cell_caches_written_by_one_page_serve_the_next`(두 번째 페이지가 첫 페이지의 항목으로 Lua 없이 같은 명령 수·픽셀, 팔레트를 바꾸면 다른 항목), `page_cache_reports_engine_writes_only`. **브라우저 측정**(내장 브라우저, Apple M5, 데모 샘플: SHOM S-101 셀 4개, SHOM S-102 7개, UKHO S-124 5개, IHO S-129 1개, 70,114 명령, 캔버스 1560×1300 물리 픽셀): 워커+WebGPU 첫 Lua 준비 2.0 s(그리기 0.2 s), 이때 메인 스레드의 가장 긴 차단 19 ms(메시지 루프 측정 `?probe=1`), 메인 스레드 경로(`?worker=off`)는 같은 준비가 2.8 s(측정을 켜면 3.1 s)이고 그동안 페이지가 멈춘다(측정한 가장 긴 차단 3.3 s). 다시 불러오기: IndexedDB에서 4개 항목(50 MB)을 0.1 s에 읽고 준비 0.33 s(캐시 적중 4, Lua 미실행), 차가운 캐시 2.0 s 대비. WebGL2(`?backend=webgl2`, 워커의 OffscreenCanvas, ANGLE Metal)도 같은 모습으로 그리고 캐시 적중 준비 0.33 s. S-124·S-129·S-101(Lua)·S-102가 WebGPU·WebGL2 모두에서 그려지고 워커 경유 피킹이 동작한다. 스크린숏으로 같은 위치의 CLI 렌더(같은 파일 묶음)와 S-102 모습이 같은 것을 확인했다. **남은 것**: 자동 전환의 실제 브라우저 확인, S-102 규칙 명령·S-101 페이로드의 IndexedDB 저장, 큰 셀 묶음의 브라우저 측정은 6e에서 완료. 메시지 루프 측정은 숨겨진 창의 애니메이션 프레임·긴 작업 보고가 멈추는 환경을 위한 것으로 한 코어를 쓴다(워커 준비가 그동안 느려짐). 워커와 페이지 사이 파일 바이트 복사(전송 아님) |
| 6e | **완료. 웹 전환 경로 확인, 영구 캐시 확대, 큰 묶음 측정, 보기 전용 방출.** **전환 시험 훅**: 데모의 `?simulate=`(쉼표로 여러 개, `host.js`의 `simulate`가 엔진·GPU를 건드리기 전에 페이지와 워커에 적용): `no-webgpu`(페이지·워커의 `navigator.gpu` 제거), `no-offscreen`(`OffscreenCanvas`·`transferControlToOffscreen` 제거), `no-worker-gpu`(워커만 WebGPU 없음), `worker-gpu-fail`(워커에 `navigator.gpu`는 있으나 `requestAdapter`가 null이라 넘겨받은 캔버스의 GPU 컨텍스트를 만들지 못함). `connect`는 전환 이유를 `fallbacks`로 돌려주고 상태 줄에 적는다. 내장 브라우저(Apple M5)에서 확인: `no-worker-gpu`는 캔버스를 넘기기 전에 메인 스레드 WebGPU로 전환("no WebGPU in workers"), `worker-gpu-fail`은 워커의 `WebCanvas.from_offscreen`이 실패("Failed to create surface for any enabled backend")한 뒤 넘겨진 캔버스를 새 요소로 바꿔 메인 스레드 WebGPU로 그리고 휠 줌이 새 캔버스에서 동작, `no-webgpu,no-offscreen`은 메인 스레드 WebGL2(ANGLE Metal). 세 경우 모두 데모 샘플을 70,114 명령으로 그렸고 WebGPU와 WebGL2 전환 화면(줌 6, 생말로)이 같은 모습이다. 고친 것: 스크립트 확인용 `host.snapshot`(그린 직후 같은 태스크에서 캔버스를 복사)이 OffscreenCanvas 없는 환경에서도 동작하도록 함. **영구 캐시 확대**: S-102 규칙 명령: `S102Options::reuse_rule_instructions`(기본 끔, 웹은 Lua 빌드에서 켬)이면 Lua 빌드도 캐시 저장소의 명령 문자열(`.ferrite_s102_*`, SHA-256 무결성, PC 출처·등심선 설정 키)을 먼저 쓰고, 없거나 손상되면 규칙을 평가해 다시 쓴다. 이 선택은 S-101 소스와 무관하다: 이전에는 페이로드를 넣은 `auto`가 S-101을 `Precomputed`로 준비하면 S-102도 저장 명령을 요구해 실패했고, 그 결과 준비 전체가 Lua로 다시 돌았다(이번에 고침). 새 API `ChartEngine::s102_cache_entry()`, `ChartEngine::precomputed_cell_entries()`(현재 표시로 열린 셀의 페이로드 항목), `s102_scene::s102_instruction_entry`. `WebEngine.missing_cache_entries()`는 S-102 항목, (Lua 선택이 아니면) 페이로드 항목을 먼저 알리고, 페이로드 이름이 있으면 그것만 돌려준다(찾은 페이로드에 따라 Lua 여부가 바뀌므로 호스트가 적재 뒤 한 번 더 부른다). 그다음 Lua로 준비할 때만(`lua_needed`: Lua 선택, 또는 `auto`인데 열린 셀의 페이로드가 메모리에 없을 때) 셀별 Lua 캐시를 알린다. 페이로드·S-102 이름은 페이지마다 한 번만 알린다. `load_cache_entry`로 받은 `.ferrite_pre_*`는 `load_precomputed`처럼 페이로드로 센다. 데모는 사용자가 놓은 페이로드를 IndexedDB에도 넣는다. IndexedDB는 버전 2로 바꿔 바이트(`entries`)와 장부(`meta`: 크기·사용 시각)를 나눴다. 이전에는 적재한 항목마다 사용 시각을 고치려 바이트째 다시 썼고 목록·정리도 모든 바이트를 읽었다(UKHO 묶음 32개 191 MB 적재 268 ms → 111 ms). 512 MiB LRU 예산은 세 종류(`lua`·`payload`·`s102`)에 함께 적용되고 `?cachebudget=MB`로 바꿀 수 있다. HUD는 적재·저장 수를 종류별로, 이번 준비의 캐시 적중 수와 wasm 메모리를 보여 준다. 브라우저 확인(데모 샘플): 페이로드 4개를 놓고 준비하면 S-101 `precomputed`(캐시 적중 4), S-102는 Lua로 평가해 항목 1개 저장, IndexedDB에는 payload 4·s102 1. 다시 불러오면 5/5를 IndexedDB에서 읽어(50 MB, 0.11 s) S-101 `precomputed`, 적중 5, 새 저장 0(Lua 미실행)이고 줌 6 화면이 놓은 페이지와 픽셀 단위로 같다. `?cachebudget=30`에서는 넣는 즉시 오래된 항목부터 지워 29.7 MB로 맞추고, 페이로드가 2개만 남은 다시 불러오기는 `auto`가 Lua로 바뀌어(빠진 페이로드) Lua 캐시 4개를 쓰고 예산에 맞게 정리한다. 테스트 `payloads_and_s102_instructions_serve_the_next_page`(내보낸 페이로드를 놓은 페이지는 `precomputed`이고 S-102 명령만 쓰며, 다시 연 페이지는 두 항목을 적재해 Lua 없이 적중 2·쓰기 0·같은 픽셀, 손상된 S-102 항목은 거부되고 다시 써짐). `cell_caches_written_by_one_page_serve_the_next`는 새 두 단계 프로토콜(페이로드 먼저, 다음에 Lua 캐시)에 맞췄다. **레지던시 시험**: `ChartEngine::set_residency_threshold(Option<usize>)`(`EngineState::residency_threshold_override`, 기본 None이면 기존 `FERRITE_RESIDENCY_THRESHOLD`/1,500,000 그대로), `WebEngine.set_residency_threshold(n)`(음수면 기본), 데모 `?residency=N`. 테스트 `set_view_follows_residency_changes`(SHOM 셀 4개, 임계값 0: 생말로 줌 60으로 들어가면 레지던트 집합이 바뀌어 다시 준비, 조금 옮기면 재투영, 개요로 나오면 다시 준비. 각 단계가 같은 뷰의 새 준비와 명령 수·레지던트 수·픽셀이 같음). **큰 묶음 측정**(내장 브라우저, WebGPU, 캔버스 1560×1600 물리 픽셀): UKHO 2026년 4월 S-101 교환 세트(셀 27개; `TestData/UKHO-S101/.../S-101 Trial Cells`의 17개는 사양 1판이라 6b처럼 열리지 않으므로 6b의 27셀 묶음을 썼다) + SHOM S-101 셀 4개 + SHOM S-102 7개 + UKHO S-124 5개 + IHO S-129 1개, 파일 177개, 데이터셋 37개, 317,528 명령. 열기 1.8 s. 오류는 S-124 4개 파일의 FC 검증 오류(속성 이름)와 S-129의 연관 다중도 경고 303개뿐이고 콘솔 오류 없음. 차가운 캐시 Lua 첫 준비: 워커 6.92 s(두 번 측정 같음), 메인 스레드 6.88 s, 첫 그리기 0.44 s(설정·삼각분할 0.25 s, 명령 방출 0.15 s), wasm 메모리 887 MiB, IndexedDB 저장 32개(Lua 31·S-102 1, 191 MB). 캐시 적중 다시 불러오기: 32개 적재 0.11 s, 준비 0.74 s(Lua 미실행), wasm 717 MiB. 팬·줌(재투영): `set_view` 21~105 ms(솔렌트 줌 32 처음 429~492 ms, 다음 212 ms), 그리기는 보기 전용 방출로 61~138 ms(같은 경로 두 번째), 각 위치를 처음 볼 때는 233~630 ms(그 위치의 선·글자 준비). 레지던시(`?residency=0`): 개요 4/31셀 57,157 명령, 줌 4 3셀, 줌 8 2셀 21,138 명령, 줌 16 재투영, 줌 32 11셀 104,639 명령, 생말로 줌 12 2셀 36,563 명령. 레지던트 집합이 바뀔 때마다 `reprojected=false`로 캐시 적중 준비(적중 수 = 레지던트 셀 + S-102, Lua 쓰기 0), 같은 집합 안의 이동은 재투영(33~145 ms). 레지던시 변경 준비는 200~450 ms, 그리기 66~283 ms(같은 순서의 네이티브 단일 스레드 115~243 ms, 그리기 45~233 ms). 이동해 들어온 줌 32 화면과 그 위치에서 새로 연 페이지의 화면은 픽셀 단위로 같다(카메라가 1.4e-7° 다른 프레임끼리만 1.04%가 안티앨리어싱으로 다름). **측정 환경 주의**: 숨겨진 브라우저 창에서는 같은 전환이 한동안 5~10배 느려지는 상태가 반복해서 나타났다(레지던시 준비 1.4~2.7 s, 그리기 0.6~1.7 s, 고정 작업인 방출 설정 0.6 → 5 ms처럼 프레임의 모든 단계가 함께 느려짐). 네이티브는 같은 순서에서 일정하고, 같은 시점에 페이지에서 만든 워커의 JS 반복 측정도 메인 스레드보다 3~4배 느렸으므로 창이 숨겨진 동안의 스레드 우선순위 등 측정 환경 탓으로 보고 위 수치는 그 상태가 아닐 때의 값이다. **보기 전용 방출**: 뷰어의 팬·줌(`rebuild_view_ex`)은 장면을 게시할 때(`emit_published_scene`) 한 번 한 일(심볼 래스터화 선행, 면 삼각분할 선행, 셀 리소스)을 다시 하지 않고 명령을 새 뷰로 다시 방출만 한다. 뷰어는 이동 중 GPU 변환(`set_pan_offset`/`set_gpu_zoom`)이나 `FERRITE_MOTION_PREVIEW`를 기본으로 쓰지 않으며, 재방출 비용 자체는 `ferrite-gpu`의 기본 켜짐 보존 캐시(경로·삼각분할·커버리지 시험 재사용 등, wasm에서도 같은 기본값)가 줄인다. `SceneEmitter`는 이제 같은 장면(`BathymetryScene::id`, 준비마다 고유)·같은 카탈로그·팔레트·표시 설정의 다음 프레임에서 렌더러 설정·팔레트·배경·편집 축척, 심볼 캐시, `prefetch_symbols`, `prefill_area_triangulations`, 뷰잉 그룹, 셀 리소스(`CellPortrayalResources`)를 다시 만들지 않고 `begin_frame` → S-102 재투영 → 경도 감기 → 명령 방출만 한다. 단계 시간은 `SceneEmitter::timings()`(`EmitTimings`), 웹 `WebCanvas.emit_timings()`, 비교용 `set_view_only_reuse(false)`/데모 `?viewreuse=0`, `OffscreenChartRenderer::emitter()`. 네이티브 뷰어 코드는 바뀌지 않았다. 네이티브 확인(UKHO 27셀, 1560×1300, 단일 스레드): 재사용 켜고 끈 렌더의 이미지 합이 모든 위치에서 같고, 재투영 프레임 방출이 4.5 ms 줄었다(설정 0.3 + 선행 4.2 ms). 브라우저(317,528 명령, 같은 위치 순서, 두 번째 반복): 이전 그리기 122~183 ms(설정·선행 매 프레임 42~53 ms) → 61~138 ms, 위치 열 곳 평균 146 ms → 95~99 ms(메인 스레드·워커). 남은 그리기 시간은 명령 방출 자체(53~109 ms)로 뷰어도 매 프레임 하는 일이고, 같은 장면의 네이티브 방출(2~40 ms)보다 크게 느리다. **남은 것**: wasm 명령 방출과 캐시 적중 준비가 네이티브보다 몇 배 느린 원인 분석(프로파일러로 함수별 확인), 이동 중 GPU 변환을 쓰는 미리보기(뷰어도 기본으로 쓰지 않음), 숨겨진 창이 아닌 환경의 측정 반복, 데모가 카메라 되먹임으로 같은 위치를 한 번 더 그리는 경우(재투영 프레임 한 장, 위치 1e-7° 이동) |
| 6f | **완료. wasm 방출·캐시 적중 준비 프로파일링, 캐시 무결성 검사 재사용, 측정 환경 확인.** **방법**: 네이티브는 웹과 같은 `WebEngineCore` 경로를 도는 임시 측정 예제(UKHO 27셀 묶음 317,528 명령, 1560×1600, 밀도 2, `RAYON_NUM_THREADS=1`, 위치 다섯 곳을 번갈아 재투영·방출)와 macOS `sample`, wasm은 이름 섹션을 남긴 빌드(`CARGO_PROFILE_RELEASE_STRIP=none`)를 내장 브라우저 메인 스레드에서 JS Self-Profiling API(`Document-Policy: js-profiling`)로, 캐시 적중 준비는 같은 빌드를 Node `--cpu-prof`로 측정했다. 빌드끼리 비교는 한 페이지에 두 wasm 모듈을 올려 같은 프레임을 번갈아 그리는 측정(측정 환경 변동이 양쪽에 같이 걸림)으로 했다. **명령 방출**: 같은 일을 쉬지 않고 반복하면 wasm 방출 10.8~11.1 ms(중앙값), `set_view` 재투영 6.6~7 ms, 네이티브 단일 스레드 5.6 ms / 5.4 ms로 약 2배·1.3배다. 함수별 분포는 두 쪽이 비슷하고(방출 본체, 메르카토르 libm 약 14%, u32 정렬 7~10%, 해시 wasm 8%·네이티브 3%, wasm은 WebGPU JS 호출 `writeTexture`·바인드 그룹 등 약 10%, dlmalloc 1~3%, rayon 단일 스레드 대체는 래퍼 프레임뿐) 한 곳이 지배하지 않는 wasm 코드 생성의 일반적 비용이다. 시험한 것: `+simd128`(±1%), 삼각분할 캐시의 Fx 해시, 셀 서수 병합을 정렬 대신 표시 배열로(네이티브·wasm 모두 측정 오차 안) 모두 이득이 없어 넣지 않았다. 릴리스 프로필은 이미 opt-level 3·fat LTO·codegen-units 1이고, wasm32에는 되감기가 없어 panic 설정은 코드에 영향이 없다. **6e의 53~109 ms는 측정 환경 탓**: 같은 프레임을 쉬지 않고 그리면 방출 10.8 ms·`set_view` 6.6 ms, 프레임 사이에 100 ms 타이머 대기를 두면(데모의 프레임 흐름처럼) 67.9 ms·44.6 ms(6.3배)였다. 이때 페이지의 JS 반복 측정(2e7회)은 196~311 ms로, 네이티브 Node의 81 ms가 `taskpolicy -b`(백그라운드 QoS, 효율 코어)에서 310~330 ms가 되는 것과 같다. 창이 가려진 상태(창 패널은 표시했으나 앱 창이 앞에 있지 않아 `requestAnimationFrame` 두 번에 5.2 s, 타이머가 약 1 s로 묶임)에서는 렌더러가 쉬었다 깨어날 때 효율 코어 속도로 돈다. 이번에도 창을 앞으로 가져올 수 없어, 아래 브라우저 수치는 JS 측정이 성능 코어 속도(85 ms 안팎)인 구간에서 같은 페이지 안 번갈아 측정한 값이다. **캐시 적중 준비**: Node 프로파일에서 준비 시간의 55%가 캐시 항목(32개, 191 MB)의 SHA-256 무결성 검사였다. 네이티브도 `ferrite-engine`을 단독으로 빌드하면(CLI, C ABI, 예제) `sha2`의 ARMv8 SHA 명령(`asm`)이 루트 패키지에서만 켜져 소프트웨어 SHA-256으로 돌아 570 ms 중 340 ms가 검사였다. 고친 것(네이티브 출력 그대로): (1) `CacheStore::read_entry`/`mark_verified`(기본 구현은 매번 검사, 파일 저장소는 그대로)와 `MemoryCacheStore`가 쓰기 단위로 검사 통과를 기억(`CacheEntry::version`, 다시 쓰면 해제, `write_verified`). `portrayal_cache::read_and_deserialize_cache`가 셀 캐시와 사전 계산 페이로드를 읽을 때 기억된 항목은 SHA-256을 건너뛰고(매직·스키마 버전·디코딩 검사는 그대로) 통과하면 기록한다. (2) 웹: `WebEngine.load_cache_entry_digest(name, bytes, sha256)`(`WebEngineCore::load_cache_entry_checked`, `portrayal_cache::stored_digest`): 호스트가 Web Crypto로 계산한 페이로드 해시가 항목에 저장된 해시와 같으면 검사된 항목으로 넣는다. `host.js`의 `preload`가 IndexedDB에서 읽은 항목을 `crypto.subtle.digest`로 병렬 해시해 넘긴다(보안 컨텍스트가 아니면 기존 경로). (3) `ferrite-engine`도 aarch64(MSVC 제외)에서 `sha2`의 `asm`을 켠다(루트와 같은 선택, Cargo.lock 변화 없음). 테스트 `memory_store_remembers_integrity_checks_per_write`, `host_digest_marks_matching_entries_checked`. **결과**: 브라우저(한 페이지 번갈아, 메인 스레드): 6e 빌드 캐시 적중 준비 668~674 ms(매번), 이번 빌드는 Web Crypto 해시 75~84 ms + 준비 305~327 ms(해시 없이 넣으면 첫 준비 670 ms, 다음부터 310 ms). 차가운 Lua 준비는 두 빌드 모두 6.97 s. 레지던시 변경 준비(`residency 0`, 같은 위치 순서 세 번): 중앙값 231 → 189 ms, 줌 32(14셀, 127,203 명령) 371~373 → 247~248 ms. 레지던시가 바뀌지 않는 이동은 재투영 6~7 ms. 데모 워커 다시 불러오기: IndexedDB 32개 191 MB 적재 180 ms(해시 74 ms 포함), 준비 343 ms(6e 0.11 s + 0.74 s), 첫 그리기 448 ms, wasm 717 MiB(첫 Lua 준비 뒤 888 MiB). Node: 반복 캐시 적중 준비 690 → 305 ms, 해시를 넘긴 첫 준비 379 ms. 네이티브 측정 예제(단일 스레드): 첫 캐시 적중 준비 570 → 305 ms(하드웨어 SHA), 반복 550 → 220 ms(메모리 저장소의 검사 기억), 방출 5.6 ms 그대로, 모든 위치의 이미지 합 동일. **남은 것**: 창이 앞에 있는 환경에서 데모 자체의 프레임 흐름(워커, `requestAnimationFrame`) 수치 반복, wasm 방출의 약 2배(코드 생성 일반 비용; WebGPU 호출 묶기, 좌표 변환의 libm 호출 줄이기는 네이티브에도 걸리는 별도 작업), 같은 위치의 재투영 여부가 앞선 프레임 기록에 따라 달라지는 경우 확인(많은 위치를 그린 엔진은 줌 16에서 다시 준비했고 새로 연 두 빌드는 모두 재투영) |


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

메모리 데이터는 `Platform::in_memory`나 `DatasetSource::Bytes`로 연다. `SignaturePolicy::Required`와 트러스트 앵커를 주면 메모리의 S-101 교환 세트도 네이티브 폴더와 같이 검증·계획하고(절대 경로 이름), 명시적으로 연 취소는 플랫폼 `UserStore`의 취소 이력에 남는다. Lua 없이 빌드한 호스트는
네이티브 빌드가 `set_precomputed_export(true)`(또는 `ferrite precompute`)로 만든 페이로드를 캐시
저장소에 넣고 `set_portrayal_source(PortrayalSource::Precomputed)`로 그린다.

**CLI** (`cargo build -p ferrite-cli`, 실행 파일 `ferrite`):

```
ferrite inspect  [--json] PATH...                  # 데이터셋, 피처 유형별 개수, 검증 문제
ferrite validate [--json] [--verify] PATH...       # 검증 + 교환 세트 검사, 오류가 있으면 종료 코드 1
ferrite render PATH... --out chart.png [--center LAT,LON] [--zoom Z | --scale N]
               [--size WxH] [--density F] [--palette Day|Dusk|Night] [--precomputed DIR]
                                                   # PATH에 S-102 HDF5(.h5)도 넣으면 뷰어처럼 수심 래스터를 그림
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

**웹** (`crates/ferrite-wasm`, wasm-bindgen 0.2.108):

```
cargo install wasm-bindgen-cli --version 0.2.108 --locked --root /path/tools
ferrite precompute CELLS... --cache-dir PRE [--palette Day|Dusk|Night]   # 네이티브(Lua) 빌드
python3 -I tools/build_wasm.py --out /path/www --wasm-bindgen /path/tools/bin/wasm-bindgen \
    --target-dir /path/target-wasm [--sample FILE_OR_DIR]... [--precomputed PRE]
python3 -m http.server -d /path/www 8000      # http://localhost:8000/
```

`build_wasm.py`는 `cargo build -p ferrite-wasm --target wasm32-unknown-unknown`(기본 `lua-wasm`·`s102-pure`, `--no-lua`·`--no-s102`로 끔), `wasm-bindgen --target web`,
`www/` 복사, 카탈로그 묶음 `catalogues.bin`(S-101·S-102·S-124·S-129 FC/PC) 작성을 하고 결과는 저장소 밖에 둔다.
데모 엔진은 웹 워커에서 OffscreenCanvas로 그리고(`?worker=auto|on|off`, 안 되면 메인 스레드), 셀별 Lua 포트레이얼 캐시·S-102 규칙 명령·놓은 사전 계산 페이로드를 IndexedDB에 둔다(`?cache=0`으로 끔, `?cachebudget=MB`, "Clear portrayal cache" 버튼). HUD에 실행 위치, 종류별 캐시 적재·저장 수, 방출 시간, wasm 메모리, 메인 스레드 프레임 간격을 보여 준다. 확인용 매개변수: `?simulate=no-webgpu,no-offscreen,no-worker-gpu,worker-gpu-fail`(전환 경로), `?residency=N`(레지던시 임계값), `?viewreuse=0`(보기 전용 방출 끔).
데모는 파일 놓기(S-124/S-129 GML, S-101 셀, S-102 HDF5, `.ferrite_pre_*` 페이로드)와 `--sample` 묶음 불러오기,
드래그 팬·휠 줌·클릭 피킹·팔레트 선택과 렌더러 백엔드 선택(`?backend=auto|webgpu|webgl2`, HUD에 사용 중인 어댑터·백엔드 표시)을 제공한다. 사전 계산 페이로드의 키에는 팔레트가 들어가므로
쓸 팔레트마다 `precompute`를 돌린다. 기본 빌드(`lua-wasm`)는 S-101을 브라우저에서 Lua로 포트레이얼하므로 페이로드 없이 셀만
넣어도 된다. 웹 Lua와 네이티브의 Part 9a 출력 동일성은 `python3 -I tools/lua_wasm_parity.py --pkg OUT/pkg --work DIR`로 확인한다.

```js
import init, { WebEngine, WebCanvas } from "./pkg/ferrite_wasm.js";
await init();
const engine = WebEngine.from_bundle(new Uint8Array(await (await fetch("catalogues.bin")).arrayBuffer()));
engine.load_precomputed(".ferrite_pre_<key>.bin", payloadBytes);
engine.add_file("101FR00369400.000", cellBytes);
engine.add_file("124GB00S1Y2026N36.GML", gmlBytes);
const report = JSON.parse(engine.open());
const gpu = await WebCanvas.create("chart", devicePixelRatio, "auto");   // 캔버스 width/height = 물리 픽셀; "webgpu" | "webgl2"
engine.set_portrayal("auto");                                          // "lua" | "precomputed"
engine.prepare(canvas.width, canvas.height, devicePixelRatio, 49.21, -2.13, 20, false);
engine.render_to_canvas(gpu);
// 팬·줌: 레지던트 셀이 같으면 다시 포트레이얼하지 않고 재투영(요약의 reprojected)
engine.set_view(canvas.width, canvas.height, devicePixelRatio, 49.22, -2.11, 30, false);
engine.render_to_canvas(gpu);
const hits = JSON.parse(engine.pick_json(x * devicePixelRatio, y * devicePixelRatio, 8));
```

웹 워커에서는 페이지가 캔버스를 넘기고 워커가 `WebCanvas.from_offscreen`으로 그린다. 영구 캐시는 준비 전후에 엔진 밖에서 옮긴다(데모의 `www/host.js`).

```js
// 페이지
const offscreen = canvas.transferControlToOffscreen();
worker.postMessage({ canvas: offscreen, density: devicePixelRatio }, [offscreen]);
// 워커
const gpu = await WebCanvas.from_offscreen(offscreen, density, "auto");
if (!engine.scene_current()) {                                          // 재투영이 아니면 포트레이얼
  for (let round = 0; round < 2; round++) {                             // 페이로드·S-102 먼저, 그다음 Lua 캐시
    const names = JSON.parse(engine.missing_cache_entries());
    for (const [name, bytes] of await idb.getMany(names)) {            // Web Crypto 해시를 넘기면 wasm SHA-256 생략
      const sha = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes.subarray(40)));
      engine.load_cache_entry_digest(name, bytes, sha);                 // 또는 load_cache_entry(name, bytes)
    }
    if (!names.some((n) => n.startsWith(".ferrite_pre_"))) break;
  }
}
engine.set_view(width, height, density, lat, lon, zoom, false);        // 요약의 cache_reads
engine.render_to_canvas(gpu);
await idb.putMany(engine.take_cache_writes());                          // [[이름, Uint8Array]...]
```

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

## 7. 제품 모듈화

### 현재 구조와 문제

크레이트 분리(1~6단계)는 계층(엔진·GPU·UI·애플리케이션)을 나눴지만 제품 코드는 계층마다 흩어져 있다.

- S-101: `ferrite-s101`(셀 읽기·Lua 변환) 외에 엔진의 `s101_*` 모듈(업데이트 계획, 생명주기, 교환 세트 검사), `EngineState`의 셀·카탈로그 필드, `chart_engine.rs`의 열기·준비 분기(4단계에서 `ferrite_s101::layer`로 모음).
- S-102: `ferrite-s102`, 엔진 `s102_*`(장면, 수심 정책, 입력 포착, 저널 허용, 시험 워커)와 `EngineState`의 `s102_*` 필드, `ferrite-gpu` 래스터 경로, `src/main.rs` 분기(약 230곳).
- S-421: `ferrite-s421`, 엔진 `s421_dataset_input`, `ferrite-gpu`의 `native_route_*`, `ferrite-ui` 경로 패널, `src/native_s421.rs`(3단계에서 `ferrite-s421`로 모음).
- S-124/S-129: 두 제품이 `ferrite-s100-gml` 한 크레이트에 섞여 있고, 생명주기·사용자 상태·포트레이얼 옵션은 엔진 `gml_overlay.rs`와 `src/gml_overlay.rs`에 `Product::S124`/`S129` 분기로 들어 있었다.

제품 하나를 추가하거나 빼려면 여러 크레이트를 함께 고쳐야 하고 제품 단위로 빌드할 수 없다.

### 목표 구조

6단계까지 마친 구조다.

```
                       ferrite-product-api
        ProductAdapter · ProductLayer · open(OpenStage) · report · platform
              ▲              ▲               ▲               ▲
   ┌──────────┴───┐  ┌───────┴──────┐  ┌─────┴──────┐  ┌─────┴──────┐
   ferrite-s101    ferrite-s102     ferrite-s421    ferrite-s124/-s129
   (S101Layer)     (S102Layer)      (S421Layer)       └ ferrite-s100-gml
        │              ▲               ▲               (GML 공통 기반,
        └─ ferrite-part9a (Part 9a 명령 변환) ─┐        등록된 GmlIdentity)
        ▲              │               │       │       ▲
        └──────────────┴───────┬───────┴───────┴───────┘
                               │ 기능 플래그 s101·s102-pure·s421·s124·s129
                ferrite-engine │ products.rs (유일한 등록 지점)
     EngineState { layers: ProductLayers(Box<dyn ProductLayer>), … }
     ChartEngine: 열기 작업(단계별 sniff·accept·load) → install → 보고·합성
                               │
     ┌──────────────┬──────────┴──────┬──────────────┐
  ferrite-cli   ferrite-capi    ferrite-wasm    ferrite-s100(앱)
                                                  └ ferrite-ui: PanelRegistry
                                                    (제품 패널 데이터 모델)
```

```
ferrite-product-api   공통 인터페이스: ProductAdapter, ProductLayer, 열기 작업 단계
                      (open), 공통 보고 모델(report), 데이터 모델, 제품이 쓰는 플랫폼
                      서비스(Clock, UserRecord, UserStore, DataSource, CacheStore,
                      Executor). egui·wgpu·서명 백엔드 비의존
ferrite-s100-gml      GML 공통 기반: Part 10b 판독기, FC 검증, 교환 세트 검증,
                      Part 9 XSLT 입력·변환, 오버레이 계층 상태, GML 제품 인터페이스.
                      제품 이름은 모른다(등록된 제품의 GmlIdentity·GmlRules를 받는다)
ferrite-part9a        제품 비의존 Part 9a 명령 변환(Lua 결과 → DrawingInstruction).
                      도형은 GeometrySource(S-101 셀, GML 합성 레코드)에서 읽는다
ferrite-s101 / -s102 / -s124 / -s129 / -s421
                      제품별 크레이트. 제품 고유 코드(판별, 열기 단계, 설치, 보고,
                      포트레이얼, 피킹, 패널 모델, 사용자 기록 형식)를 모두 가진다
ferrite-engine        제품 비의존: 등록된 계층의 저장소(ProductLayers), 열기 작업,
                      장면 합성(역할별), 커버리지, 캐시, 플랫폼 서비스. 제품 크레이트는
                      기능 플래그 뒤에서 products.rs가 등록
ferrite-ui            패널 등록부(PanelRegistry)가 등록된 제품의 패널 데이터 모델을 그린다
                      (S-421 경로 패널, S-124 경고 목록, S-129 계획 창)
```

### 인터페이스 (`ferrite-product-api`)

| 항목 | 내용 |
|---|---|
| `ProductAdapter` | 제품 하나. `info()`(`ProductId`, 이름, 인코딩, 장면 역할 `SceneRole`), `sniff(name, head)`(파일 이름과 앞 64 KiB로 데이터셋·가져오기 형식 판별), `import_extensions()`·`imports_archives()`, `panels()`(`PanelDescriptor`), `as_any()`/`as_any_mut()`(제품을 아는 호스트가 제품 고유 기능에 접근) |
| `ProductLayer` | 같은 인코딩·포트레이얼 경로를 쓰는 제품들의 데이터셋 저장소(`Any`, 형식 접근은 `as_any`). `open(DatasetInput)`(이름, 바이트(검증 시 인증 스냅숏), 갱신 체인 `updates`, `Provenance`, 서명 정책 `signatures`; `check_policy`로 정책 확인), `remove`, `set_visible`, `portray(PortrayalRequest)` → `Result<Option<SceneContribution>>`(리비전, `DrawingInstruction`, `RendererExtension`), `coverage`, `describe(key)` → `FeaturePick`, `describe_in(dataset, key)` → `FeatureReport`(셀 색인이 있는 계층), `highlight(key)`, `portrayal_catalogue`(장면을 그리는 설치된 PC: 호스트는 등록 순서의 첫 계층 것을 쓴다). 열기 작업: `open_stage` → `OpenStage`, `check_opened`, `install_early`, `install`. 보고: `datasets`, `feature_types`, `validation_issues`, `exchange_set_checks`. 호스트 검사: `check_display`(색상 프로필·설정), `numeric_coverages`(S-98 Level 2 입력) |
| `PortrayalRequest` | 프로필, 보기 시각, 표시 설정과 호스트 서비스 `HostServices`(규칙 출처 `PortrayalSource`(Lua/사전 계산), 사전 계산 내보내기, 캐시 저장소, 상호운용 `Interoperability`(카탈로그와 제품별 선택을 `Any`로 넘김: API가 서명 백엔드에 의존하지 않도록)) |
| `open` | 호스트 열기 작업의 단계 계약. `OpenStage`(`sniffs_head`·`sniff`(어댑터 판별), `accept`, `counted_inputs`, `after_claims`, `load` → `Loaded`(페이로드와 종류별 개수)), `OpenContext`(데이터 원천, 실행기, `SignaturePolicy`, 명시 선택 여부, 인증 함수 `Authorize`(`ferrite_security::AuthorizedDatasets`를 `Any`로), 사용자 저장소, 취소), `OpenNotes`, `InstallContext` |
| `report` | 공통 보고 모델: `DatasetId`, `DatasetSummary`, `IssueSeverity`, `ValidationIssue`, `FeatureReport`, `InstallReport`, 제품 고유 보고 `ProductReport`(제품·종류·페이로드; S-101 취소 기록, S-102 파일 요약) |
| `RendererExtension` | 렌더러가 아는 종류 이름과 페이로드. S-102 래스터 타일, S-421 경로 오버레이를 그리기 명령 밖의 데이터로 넘기는 자리 |
| 패널 데이터 모델 | 제품 크레이트가 행·옵션 타입을 정의하고(`ferrite_s124::panel`, `ferrite_s129::panel`), UI 크레이트는 그 타입만 그린다. API 크레이트는 egui에 의존하지 않는다 |
| `platform::{Clock, UserRecord, UserStore, RecordKind, FileRecord, DataSource, CacheStore, Executor, Services}` | 제품이 직접 쓰는 플랫폼 서비스(시각, 사용자별 기록, 데이터 원천, 캐시 저장소, 실행기)와 계층이 보관하는 핸들 묶음. `UserStore::product_state(product, key)`·`product_state_path`는 제품별 기록이고, 기록 형식(파일 기록, 표준 위치, 메모리 키)은 제품이 `RecordKind`로 준다(5단계). 로컬 파일·메모리 데이터 원천(`NativeDataSource`, `MemoryDataSource`), 순차 실행기, `parallel_map`, 캐시 항목 한도도 여기 있다(4단계). `ferrite_engine::platform`이 다시 내보내고 나머지를 구현한다 |
| `CatalogueLayout`, `Provenance::Imported { format, product }` | 제품이 주는 문구: 카탈로그 폴더가 없을 때의 설치 위치(`ferrite_s101::layer::catalogue::LAYOUT`, `ferrite_s102::layer::CATALOGUE_LAYOUT`), 가져온 데이터셋의 출처 표시("not an official {product} dataset") |
| `route_overlay` | 경로 오버레이 렌더러 확장(종류 `"S-421/route-overlay"`, 페이로드 `NativeRouteWorldSamples`): 경로별 웨이포인트(`NativeRoutePath`), 선언된 구간(`NativeRouteLegDeclaration`), 그 WGS84 표본, 편집 PC 판독기 계약(`EditingStyleReader`). 제품(생산)과 렌더러(소비)가 함께 쓴다 |

GML 제품은 `ferrite_s100_gml::product`의 두 트레이트를 더 구현한다.

- `GmlRules`(정적 값, 예: `ferrite_s124::S124`): 제품 식별(`identity()` → `GmlIdentity`: `ProductId`, productIdentifier 표기, 제품 코드(파일 이름 접두어·네임스페이스 `S<code>`), 구성원 뒤 식별 정보의 검사 식별자(S-129 `S-158-129/15`)), FC 규칙의 검사 식별자·심각도(`FcRule`), 파일 이름 규칙, 제품 규칙(`product_checks`), 교환 세트 규칙(`ExchangeRule`, 필수 메타데이터, 목적, 크기, 지원 파일), 가져오기 확장자, 오버레이 모듈 생성.
- `GmlOverlayProduct: ProductAdapter`(엔진이 소유하는 상태): 선언되지 않은 보기 그룹, 카탈로그 적재 후 처리, 포트레이얼 입력 옵션(가상 피처), XSLT 문맥 매개변수와 변환 옵션(`PortrayalSettings`), 가져오기, 수신 시각, 생명주기 평가, 피처 억제, 피킹 행·관련 정보·강조, 목록 선택, 오류 정정 옵션, 사용자별 상태. 모든 훅에 중립 기본값이 있다.

다섯 제품에 대응시키면 다음과 같다(2~5단계에서 구현).

| 요소 | S-101 | S-102 | S-124 / S-129 | S-421 |
|---|---|---|---|---|
| `sniff` | ISO 8211 `.000`/`.00n` | HDF5, productSpecification | GML 네임스페이스·productIdentifier | GML S-421 네임스페이스 |
| `open` | 교환 세트 계획, 업데이트 체인, 서명 정책 | 순수 Rust 또는 C HDF5 판독 | 판독·FC 검증·제품 규칙 | 경로 판독 |
| `portray` | Lua 규칙·셀 캐시 → `DrawingInstruction` | PC 규칙 → `RendererExtension`(래스터 타일) | XSLT → `DrawingInstruction`(셀 없음, 커버리지 예외) | `RendererExtension`(경로 오버레이) |
| `coverage` | M_COVR | 격자 범위 | 객체 범위 | 경로 범위 |
| `describe` | 피처 속성·FC 이름 | 격자 값 | FC 속성, 경고 상태 | 웨이포인트·구간 |
| 패널 | 카탈로그·셀 | 수심 옵션 | 경고 목록 / 계획 | 경로 편집 |

### 기능 플래그

`ferrite-engine`의 `s101`, `s124`, `s129`, `s421`(기본 켜짐)과 `s102-pure`/`s102-hdf5`가 제품 크레이트를 등록한다(`ferrite_engine::products`, 엔진에서 제품을 이름으로 부르는 유일한 곳; GML 제품의 식별은 `gml_products()`가 등록된 `GmlIdentity`로 판독기·판별·교환 세트 검증에 넘긴다). 끄면 그 제품의 데이터셋은 탐색·판별·열기 대상이 아니고 카탈로그도 적재하지 않는다(`s421`을 끄면 경로를 탐색·캡처하지 않고 `ChartEngine::routes`가 없다. `s101`을 끄면 `ChartEngine`은 S-101 카탈로그 없이 만들어지고 장면에 셀·커버리지가 없다. S-102가 없으면 HDF5 입력은 "not built" 알림이다). Part 9a 명령 변환은 제품 비의존 `ferrite-part9a`에 있어 `s124`/`s129`는 `ferrite-s101`을 연결하지 않는다(6단계). `ferrite-cli`·`ferrite-capi`·`ferrite-wasm`은 같은 이름의 기능을 기본으로 켜서 넘기고, 뷰어는 엔진 의존에 다섯 기능을 명시한다. CLI의 장면 출력은 등록 순서에서 처음으로 설치된 PC가 있는 계층의 PC(`ProductLayers::scene_catalogue`: `s101`이 있으면 언제나 S-101 PC)로 팔레트·기호를 정하고, S-101 고유 준비(생산 축척, 표시 모드 보기 그룹, 카탈로그별 셀 자원, 얕은 수심 무늬 선택자)는 `s101`에서만 한다. 그래서 CLI는 `s101` 없이(예: `s124`만) 빌드되고 그린다(보기 그룹 거르기 없음). `ferrite-ui`의 `s124`, `s129`, `s421`(기본 켜짐)은 패널(경고 목록, 계획 창, 경로 패널)과 경로 편집 자원 판독기를 넣는다. `tools/build_wasm.py --no-s102`는 `s101,s124,s129,s421`을 유지한다.

### 단계

각 단계의 완료 조건은 기존 검사 전체(`cargo fmt --check`, `cargo clippy -- -D warnings`, 작업 공간·wasm·기능 끈 엔진 빌드, 테스트 이름 보존, 픽셀 회귀 동일)다.

| 단계 | 내용 |
|---|---|
| 1 | **완료.** `ferrite-product-api` 정의. S-124·S-129를 `ferrite-s124`·`ferrite-s129`로 분리하고 엔진 오버레이 경로가 등록된 어댑터를 거치게 함(아래). |
| 2 | **완료.** S-102: 엔진 `s102_*` 모듈과 `EngineState`의 `s102_*` 필드를 `ferrite-s102`(제품 계층 `layer`)로 옮기고 래스터 타일을 `RendererExtension`으로 넘김. `main.rs`의 S-102 분기 일부를 계층 호출로 바꿈(아래). |
| 3 | **완료.** S-421: `s421_dataset_input`, `src/native_s421.rs`의 제품 로직을 `ferrite-s421`로 모으고, `ferrite-gpu` `native_route_*`의 데이터 모델을 경로 오버레이 확장(`ferrite_product_api::route_overlay`)으로 옮겨 경로 오버레이를 확장으로 넘긴다(아래). |
| 4 | **완료.** S-101: 엔진 `s101_*`(업데이트 계획, 생명주기, 교환 세트 검사, 카탈로그 선택)와 셀 포트레이얼의 제품 고유 부분을 `ferrite-s101` 제품 계층(`layer`)으로 옮기고 `EngineState`의 S-101 필드를 `S101Layer`로 모은다(아래). |
| 5 | **완료.** 엔진·UI 정리: `EngineState`가 등록된 계층을 `ProductLayers`(`Box<dyn ProductLayer>`)로 가지고 `ChartEngine`이 열기·설치·보고·장면 합성·피킹을 계층 인터페이스로만 한다. 열기 작업은 등록된 어댑터의 판별을 쓴다. 엔진 `s101` 기능, UI 패널 등록부와 UI 제품 기능, 제품별 사용자 기록. |
| 6 | **완료(뷰어 열기 경로 제외, 아래 남은 일).** 공통 크레이트에서 제품 지식 제거: `ferrite_s100_gml::Product` 열거형 → 등록된 `GmlIdentity`, Part 9a 변환 → `ferrite-part9a`, S-421 경로 패널 → 등록된 `ProductPanel`, CLI `s101` 없이 빌드, 제품이 주는 문구. |

### 1단계 결과

- 새 크레이트: `ferrite-product-api`, `ferrite-s124`, `ferrite-s129`.
- 옮긴 파일(`git mv`): `ferrite-s100-gml/src/{navwarn,import}.rs` → `ferrite-s124/src/`, `ferrite-s100-gml/tests/{navwarn_lifecycle,import_shom_ping}.rs` → `ferrite-s124/tests/`.
- `ferrite-s100-gml`에 남은 것: 판독기, 제품 비의존 FC 검증(`Validator`, `FcRule`, `IssueSink`, `file_name_follows`), 교환 세트 검증 절차(`ExchangeRule`), XSLT 입력·변환(가상 피처 훅 `InputOptions::virtual_features`, 오류 정정 메커니즘 `ConvertOptions`), 오버레이 계층 상태(`overlay::{OverlayCore, OverlayDataset}`, 판별 `sniff_head`), GML 제품 인터페이스(`product`).
- `ferrite-s124`: 생명주기(`navwarn`), 가져오기(`import`), 규칙(`rules`: S-124 검증 4·8.1·9.3·9.6, 교환 세트 9.5~9.8·12), 오버레이 모듈(`overlay::S124Overlay`: 평가, 분류, 표시 표시, 억제, 강조, 피킹 행), 사용자 상태 기록(`user_state`, 형식 버전 1과 위치 그대로), 경고 목록 모델(`panel`).
- `ferrite-s129`: S-158-129 규칙(`rules`), 가상 피처(`virtual_features`: `PlannedRoute`, 정보 상자), 오버레이 모듈(`S129Overlay`: PlainBoundaries, scaleMinimum, 오류 정정), 계획 패널 모델(`panel`).
- 엔진: `gml_overlay::GmlOverlays`는 등록된 모듈·카탈로그·데이터셋(`OverlayCore`, `Deref`)을 가진 제품 비의존 호스트이고 `ProductLayer`를 구현한다. `ChartEngine`의 오버레이 열기·포트레이얼은 `ProductLayer::open`/`portray`를 거친다. 제품을 아는 호스트는 `product::<T>()`/`product_mut::<T>()`를 쓴다.
- 앱·UI: `src/gml_overlay.rs`는 등록된 모듈로 행을 만들고 요청을 넘긴다. `ferrite-ui::overlay_ui`는 제품 크레이트의 패널 타입을 다시 내보내 그린다.
- 동작: 검증 문제, 포트레이얼 출력, UI, NAVWARN 상태 파일 형식·위치는 그대로다. 로그 한 줄("Correct known PC errata: on (...)")의 나열 순서가 등록 순서(S-124, S-129)로 바뀌었다.

### 2단계 결과

- 옮긴 파일(`git mv`): `ferrite-engine/src/s102_{depth_policy,input_capture,journal_admission,original_inputs,trial_worker,scene}.rs` → `ferrite-s102/src/layer/{depth_policy,input_capture,journal_admission,original_inputs,trial_worker,scene}.rs`. 엔진 `interoperability::compose_raster`(S-102 래스터 전용)도 `ferrite_s102::layer::compose_raster`로 옮겼다.
- `ferrite_s102::layer`: `S102Layer`가 `EngineState`의 S-102 필드를 모두 가진다(`fc`/`pc`와 상태, `trials`, `bathymetry`, `bathymetry_bounds`, `provenance`, `depth_policies`, `depth_inputs`, `original_inputs`, `journal_gate`). 파일 단위 조작(`insert_coverages`, `add_trial`/`remove_trial`, `take_file`/`restore_file`/`forget_file`, `clear_coverages`), 카탈로그 상태(`set_fc`/`fc_failed`/`set_pc`/`pc_failed`/`pc_status_of`), 저널 재검사(`recheck_journal_admission`), 수심 조회(`inspect`, 뷰어의 수심 피킹 문구 그대로), 열기(`open_inputs` → `install` → `Installed`), 보고(`summaries`), 포트레이얼(`prepare(S102Portrayal)` → `BathymetryScene`)을 제공한다. `S102`가 `ProductAdapter`(HDF5 서명·확장자 판별, `SceneRole::Grid`), `S102Layer`가 `ProductLayer`를 구현한다.
- 제품 크레이트의 의존: `ferrite-product-api`(엔진 비의존). 엔진 서비스는 API로 옮기거나 인자로 받는다. `DataSource`·`EntryKind`·`SourceEntry`·`CacheStore`·`CacheEntry`는 `ferrite_product_api::platform`으로 옮기고 엔진이 다시 내보낸다(경로 그대로). 서명 스냅숏 조회와 서명 끔 보고서는 `ferrite_security::AuthorizedDatasets::{snapshot_for, unchecked}`로 옮기고 엔진 `dataset_signature_policy`는 이를 부른다(오류 문구 동일). 규칙 출처(Lua/사전 계산), 내보내기, 캐시 저장소, 상호운용 선택은 `S102Portrayal`로 받는다.
- 엔진: `EngineState::s102: S102Layer`(`s102-pure`), `EngineState::bathymetry_extents()`(기능과 무관한 범위 조회). `s102_host`(구 `s102_scene`의 엔진 쪽)가 계층 결과를 `OpenReport`·`ValidationIssue`로 옮기고, `s102-pure`가 꺼진 빌드에서는 같은 이름의 평범한 자료형(`S102Options`, `BathymetrySummary`, `OpenedS102`)과 "not built" 알림을 준다. `ChartEngine::set_s102_options`는 계층의 `options`에도 반영한다.
- 렌더러 확장: `PreparedScene::bathymetry`를 `PreparedScene::{id, extensions}`로 바꿨다. S-102 타일은 종류 `ferrite_s102::layer::BATHYMETRY_EXTENSION`("S-102/bathymetry-tiles"), 페이로드 `BathymetryScene`(타일은 `Mutex`, `take_tiles`로 GPU에 넘김)이다. 장면 식별자는 `BathymetryScene::id` 대신 `PreparedScene::id`가 맡는다. `ferrite-cli`의 `SceneEmitter`는 확장을 찾아 내려받고(`s102-pure`에서 `ferrite-s102` 의존), 같은 장면의 재투영 프레임·다른 렌더러 거부 규칙은 그대로다.
- 뷰어(`src/main.rs`): 필드 접근은 `self.engine.s102.*`로, 엔진 모듈 경로는 `ferrite_s102::layer::*`로 바꿨다. 카탈로그 초기화, 시험 파일 추가·제거, 저널 재검사, 파일 게시·제거(되돌리기 포함)·전체 해제, 데이터셋 트리의 파일 목록, 수심 조회, 보기 그룹·수심 설정 변환은 계층 호출이다. GPU 단계와 얽힌 래스터 게시·재구성(`load_bathymetry_transaction`, 장면 재구성, 감사 도구)은 앱에 남겼다.
- 테스트 이동: 엔진 단위 테스트 `s102_{depth_policy,input_capture,journal_admission,original_inputs,trial_worker}::tests::*`, `s102_scene::opened::tests::*`는 `ferrite-s102`의 `layer::{...}::tests::*`, `layer::scene::tests::*`로 옮겼다(이름 그대로). 메모리 데이터 원천을 쓰던 시험은 계층의 시험용 원천(`layer::test_support`)을 쓴다. 엔진 통합 시험 `in_memory_s102`는 확장을 읽는다.
- 동작: S-102 열기·검증 문제·알림 순서, 포트레이얼 출력, 저널·시험 파일 처리, 수심 정책, 캐시 항목(키·형식), 서명 처리는 그대로다.

### 3단계 결과

- 옮긴 파일(`git mv`): `ferrite-engine/src/s421_dataset_input.rs` → `ferrite-s421/src/layer/input.rs`, `ferrite-gpu/src/native_route_leg.rs` → `ferrite-product-api/src/route_overlay/leg.rs`, `ferrite-s421/src/lib.rs` → `ferrite-s421/src/controller.rs`(경로 컨트롤러와 abi_stable 플러그인 구현; `lib.rs`는 모듈 목록과 `RouteSettings`만 남김). `ferrite-gpu/src/native_route_overlay.rs`의 경로 데이터 모델(`NativeRouteWaypoint`, `NativeRoutePath`, `NativeRouteWorldSamples`, 경로 검사, 예산 상수)과 편집 스타일 계약 타입(`EditingStyle`, `EditingPalette`, `EditingStyleReader`)은 `ferrite_product_api::route_overlay`로 옮겼다. `src/native_s421.rs`의 제품 로직은 `ferrite_s421::editor`로 옮겼다.
- 의존 방향: `ferrite-gpu → ferrite-product-api`(경로 오버레이 확장), `ferrite-s421 → ferrite-product-api`. `ferrite-gpu`는 `ferrite-s421`에 의존하지 않는다. 확장의 데이터 모델과 WGS84 구간 표본(`sample_leg`, `NativeRouteWorldSamples::prepare`)이 API 크레이트에 있는 것은 렌더러의 화면 투영(`PreparedNativeRouteOverlay`)이 표본의 불변식(표본 정책 `MAX_STEP_M`·정점 한도, 원래 경로·선언과의 정확 일치 `matches`)을 함께 검사하기 때문이다. 표본은 S-421 파싱과 무관한 선언 구간의 수치 모델이다. API 크레이트에 `ferrite-kernel`(측지) 의존이 생겼다.
- `ferrite_s421::layer`(모든 대상): `input`(경로 캡처·탐색, `DataSource`를 거침. 경로 기반 `discover_routes`는 없애고 엔진이 `discover_routes_in(&NativeDataSource, ..)`을 부름. 바이트 입력용 `capture_bytes` 추가), `S421`(`ProductAdapter`: `.gml`/`.xml`, `CATALOG.XML` 제외, 앞부분에 `S421`/`S-421`이 있으면 데이터셋 — 엔진이 쓰던 판별 그대로. `SceneRole::Route`, 패널 `S-421/routes`), `S421Layer`(`ProductLayer`: 캡처한 원본 바이트 보관, 같은 SHA-256은 한 번, 필요할 때 판독. `portray` → 그리기 명령 없는 `SceneContribution`과 경로 오버레이 확장(표시 중인 경로, 선언 구간, 리비전별 캐시), `coverage` → 웨이포인트 범위, `describe`/`highlight` → `RouteKey`(웨이포인트 양수, 구간 음수 키)의 웨이포인트·구간 보고와 구간 표본), `overlay`(경로를 확장 입력으로 바꾸는 함수, 편집기와 공용).
- `ferrite_s421::editor`(네이티브 전용, `RouteEditor`): 컨트롤러, 가져온 경로의 원본 위치, UI·기하 리비전, 경로 패널 데이터(`refresh_ui`), 가져오기(`import_path`/`import_captured`, 새 경로를 받을 때 호스트 훅), 내보내기(`export_xml`/`export_path`), 명령(`parse_event`/`apply_event`), 데이터셋 행(`RouteRow`), 카탈로그 상태, 진단(`audit_data`의 제품 부분), 경로 오버레이(`route_overlay` → `RouteOverlay::extension`, 성공한 게시 뒤 `commit_route_overlay`). `PROVIDER_ID`도 여기로 옮겼다(`ferrite_gpu::NATIVE_S421_PROVIDER`는 쓰는 곳이 없어 없앰).
- 플러그인 계층: `ferrite-s421`의 `abi_stable`·`ferrite-plugin-api`·`tempfile` 의존과 `controller`·`editor` 모듈은 wasm32가 아닌 대상에서만 빌드한다. 그래서 엔진은 `ferrite-s421`에 대상 조건 없이 의존하고(`s421` 기능), 웹 빌드의 `Profile`·`MAX_XML_BYTES` 사본을 없앴다(같은 이름의 시험은 값을 고정하는 시험으로 남김).
- `ferrite-gpu`: 화면 투영(`PreparedNativeRouteOverlay`)과 그리기 자원(`NativeRouteOverlayResources`, 편집 PC 판독기는 주입), GPU 게시(`native_route_gpu`)만 남았다. 확장 소비 `PreparedNativeRouteOverlay::from_extension(resources, &RendererExtension, scaler, ppmm)`은 페이로드의 경로·선언·리비전으로 `prepare_with_world_samples`를 부른다. 판독기 `EDITING_STYLE`은 `ferrite_s421::editing_style_contract`로 옮겼고 `ferrite_ui::native_route`는 자원 적재 래퍼와 다시 내보내기(`S421_EDITING_STYLE`)만 가진다.
- 엔진: `products::s421`(입력·어댑터·계층 다시 내보내기). `dataset_open_plan`의 탐색·단일 파일 캡처와 `ChartEngine`의 열기 작업(어댑터 `sniff`로 판별 후 캡처), 경로 저장(`S421Layer`, 중복 SHA-256은 "Already open")이 계층을 거친다. `ChartEngine::routes()`는 `Vec<&CapturedRouteInput>`을 돌려주고 `route_layer()`가 계층을 준다. 엔진의 장면(`PreparedScene`)에는 경로 확장을 넣지 않는다(이전처럼 "not portrayed by ChartEngine"; CLI·웹 렌더러는 경로 그리기 자원이 없다).
- 뷰어(`src/native_s421.rs`): 편집기 + 그리기 자원 적재(가져오기 훅에서 Day), 투영된 오버레이 캐시와 GPU 동기화, 편집 거래(포크→컨트롤러 변경→투영→GPU 게시), 파일 대화상자, 데이터셋 트리 행·카탈로그 줄 변환. 앱의 열기 계획 타입은 `ferrite_s421::layer::input`이다.
- 테스트 이동: `ferrite-engine` `s421_dataset_input::tests::*` → `ferrite-s421` `layer::input::tests::*`(발견 시험은 시험용 로컬 `DataSource`), `ferrite-gpu` `native_route_leg::tests::*` → `ferrite-product-api` `route_overlay::leg::tests::*`, `native_route_overlay::tests::*`(5개) → `route_overlay::tests::*`, `ferrite-s421` `native_import_transaction_tests::*` → `controller::native_import_transaction_tests::*`. 새 시험: `layer::tests::{adapter_sniffs_route_heads_by_name_and_namespace, layer_opens_bytes_once_and_reports_coverage_and_picks}`, `ferrite-ui` `route_overlay_extension_projects_like_its_world_samples`. 앱의 `native_s421::tests::*`는 이름 그대로 앱 쪽 연결 코드와 편집기를 함께 시험한다.
- 동작: 경로 가져오기·내보내기(원본 바이트 저장 포함), 편집 거래와 실패 시 보존, 오버레이 캐시(적중·준비 횟수, 세계 표본 재사용), GPU 게시, 패널 JSON, 진단 JSON, 알림 문구는 그대로다.

### 4단계 결과

- 옮긴 파일(`git mv`, `ferrite-engine/src/` → `ferrite-s101/src/layer/`): `s101_update_plan.rs` → `update_plan.rs`, `s101_lifecycle_metadata.rs` → `lifecycle_metadata.rs`, `s101_catalogue_selection.rs` → `catalogue_selection.rs`, `s101_xc_{capture_pool,consistency,coverage,region,authenticated_tests}.rs` → `xc_*.rs`, `chart_cells.rs` → `cells.rs`, `cell_source_identity.rs` → `source_identity.rs`, `chart_publication.rs` → `publication.rs`, `decoded_chart_cache.rs` → `decoded_cache.rs`, `portrayal.rs`, `portrayal_cache.rs`, `interoperability.rs`(S-101 벡터 명령의 S-98 합성), `primary_viewing_layers.rs`, `chart_load.rs` → `load.rs`. 나눈 파일: 엔진 `catalogue.rs`는 제품 비의존 적재(`load_feature_catalogue(_from)`, `load_portrayal_catalogue(_from)`)만 남기고 S-101 상태 검사·캐시 식별자(`validate_fc`, `validate_pc`, `hash_bound_catalogue_inputs`)는 `layer::catalogue`로, 엔진 `dataset_discovery.rs`의 재귀 탐색(`discover_recursive_files(_in)`)과 시험용 교환 세트 탐색은 `layer::discovery`로 옮기고 엔진이 다시 내보낸다. `layer::signatures`는 계획기가 쓰는 서명 보고 조회(`unchecked_datasets(_in)`, `dataset_snapshot_in`; 엔진 `dataset_signature_policy`와 같은 동작)다.
- `ferrite_s101::layer`: `S101Layer`(`layer::state`)가 `EngineState`의 S-101 필드를 모두 가진다(`fc`/`pc`와 상태, `catalogue_inventory`, `catalogue_registry`, `cells`, `loaded_source_identities`, `loaded_chain_paths`, `loaded_discovery`, `cancellation_history_path`, `coverage_inventory`/`coverage_memo`, `cell_hydration`, `cell_release_pending`, `resident_cells`, `residency_camera`, `decoded_chart_cache`, `portrayal_source`, `precomputed_export`, `residency_threshold_override`)와 플랫폼 서비스 핸들(`services`). 상태의 S-101 메서드(카탈로그 소유자 준비·조정, 캐시 경로·키, 사전 계산 키, 레지던시, 하이드레이션, 커버리지 메모, 셀 포트레이얼의 렌더러 비의존 부분)도 계층으로 옮겼다. 예전에 상태의 게시 장면·범위·색상 프로필을 읽던 메서드는 그 값을 인자로 받는다(`hash_portrayal_cache_inputs`/`cell_instruction_cache_paths`의 `profile`, `residency_for_view`·`prepare_view_residency`·`portray_loaded_cells`·`portray_change_cells`의 `view`/`bounds`). 설치 단위 조작(`begin_batch` → `insert_loaded` → `finish_batch`, `remove_cell`/`remove_dataset`, `take_cell`/`restore_cell`, `clear_cells`, `loaded_datasets`, `admit_with`)은 `ChartEngine::install`·취소 적용과 뷰어가 쓴다. `S101`이 `ProductAdapter`(체인 구성원 이름 + ISO 8211 리더 `L`, `SceneRole::BaseChart`), `S101Layer`가 `ProductLayer`(바이트의 기본 셀 열기·검증·정규화·중복/판 교체, 전체 셀 포트레이얼, 셀 범위)를 구현한다.
- 의존 방향: `ferrite-s101 → ferrite-product-api`(+ `ferrite-security`), 엔진 비의존. `ferrite_product_api::platform`으로 옮긴 것: `NativeDataSource`, `MemoryDataSource`, `memory_path`, `is_dataset_file`, `Executor`·`SequentialExecutor`·`parallel_map`, 캐시 항목 한도(`MAX_CACHE_BYTES`, `MAX_INSTRUCTIONS`), 계층이 보관하는 서비스 묶음 `Services`(데이터 원천, 캐시 저장소, 실행기). 엔진 `platform`·`secure_instruction_cache`는 같은 경로로 다시 내보낸다(`ThreadExecutor`, 캐시 저장소 구현, 사용자 저장소는 엔진에 남음). `Platform::services()`가 계층용 핸들을 준다. `ferrite-s101` 기능: `lua-native`(엔진 `lua-native`가 넘김, `lua54`/`lua55`가 켬), `test-support`(엔진 `test-support`가 넘김).
- 엔진: `EngineState::s101: S101Layer`. 엔진에 남은 것은 게시 장면(`render_context`, `bounds`, 프로필), 상호운용(`ic`, `ic_level2`, `ic_s102_selection`; `portray_loaded_cells`·`portray_change_cells`·`prepare_view_residency`는 계층을 부른 뒤 상호운용을 합성하는 얇은 메서드), S-102 계층, 서명 정책·개수(`verified_count`/`unsigned_count`는 모든 데이터셋의 인증 보고), 플랫폼. `scene`(셀 리소스 바인딩, 보기 그룹)과 `pick::describe_feature`는 엔진에 두고 계층 필드를 읽는다. `ChartEngine`의 설치·취소 적용은 계층의 설치 단위 조작을 쓰고(셀별 출처 `provenance`는 파사드에 남음) `S101Layer::admit_with`로 검증한다. `products::s101`이 어댑터와 계층을 다시 내보낸다. 엔진 루트의 `PortrayalSource`, `CellHydration`, `CellHydrationSource`, `ChartLoadResult`, `ViewportPixels`는 계층 타입을 다시 내보낸다.
- 뷰어(`src/main.rs` 외 `native_mcp`, 감사 도구)와 CLI·웹: 필드 접근은 `engine.s101.*`(`state().s101.*`)로, 모듈 경로는 `ferrite_s101::layer::*`(`ferrite_engine::{s101_update_plan, chart_publication, …}` → `ferrite_s101::layer::{update_plan, publication, …}`)로 바꿨다. 데이터셋 제거의 되돌리기와 전체 해제는 계층 호출(`take_cell`/`restore_cell`, `clear_cells`)이다. `ferrite-wasm`은 `ferrite-s101`에 직접 의존한다(캐시 페이로드 부호화, Lua 문맥 검사).
- 테스트 이동: 엔진 단위 테스트 `s101_{update_plan,lifecycle_metadata,catalogue_selection,xc_*}::…`, `chart_cells::…`, `cell_source_identity::…`, `chart_publication::…`, `decoded_chart_cache::…`, `portrayal_cache::…`, `interoperability::…`, `primary_viewing_layers::…`, `catalogue::bound_catalogue_cache_tests::…`, `dataset_discovery::tests::…`는 `ferrite-s101`의 `layer::{update_plan,lifecycle_metadata,…,catalogue,discovery}::…`로 옮겼다(이름 그대로). 엔진의 메모리 저장소를 쓰는 두 시험(`memory_store_remembers_integrity_checks_per_write`, `history_store_in_a_memory_user_store_round_trips_and_refuses_corruption`)은 엔진 `platform::tests`로 옮겼다(계층의 공개 API를 부름; `CancellationHistory::get` 추가). 새 시험: `layer::tests::{adapter_sniffs_chain_members_by_name_and_iso8211_leader, layer_opens_a_cell_once_and_reports_coverage_and_portrayal}`.
- 동작: S-101 열기·업데이트 계획·서명 처리·취소와 이력, 검증 문제와 알림 문구, 포트레이얼 출력(픽셀 회귀 동일, 차가운·따뜻한 셀 캐시), 캐시 형식·키(`CACHE_SCHEMA_VERSION` 43, 셀 캐시·사전 계산 키 유도)는 그대로다. 같은 Lua 빌드 식별자(`FERRITE_LUA_LOCK_PATH`로 같은 잠금 파일을 지정)로 빌드한 기준·작업 바이너리에서, 기준이 쓴 셀 캐시 4개를 작업 바이너리가 모두 적중으로 읽고(새 파일 없음, 화면 동일) 세 위치에서 확인했다. 의존 관계가 바뀌어 `Cargo.lock`이 달라졌으므로 보통 빌드에서는 Lua 빌드 식별자(`ferrite-lua-runtime` 빌드 지문, 잠금 파일 포함)가 바뀌어 기존 셀 캐시가 한 번 다시 만들어진다(의도된 무효화).
- `s101` 기능은 두지 않았다. `EngineState`와 `ChartEngine`(열기 작업, 설치, 준비, 피킹, 보고)이 S-101 계층을 바탕 해도로 직접 가지므로 기능으로 끄려면 엔진 전체의 분기가 필요하다. 5단계에서 엔진이 `Vec<Box<dyn ProductLayer>>`로 계층을 합성할 때 함께 둔다.

### 5단계 결과

- 인터페이스(`ferrite-product-api`): `portray`는 `Result<Option<SceneContribution>>`이고 요청에 `HostServices`(규칙 출처 `PortrayalSource`, 사전 계산 내보내기, 캐시 저장소, 상호운용)를 싣는다. `PortrayalSource`는 API로 옮겼다(`ferrite_s101::layer::PortrayalSource`는 다시 내보내기, 기본값은 `portrayal::default_source`). `DatasetInput`은 갱신 체인(`updates`)과 서명 정책(`signatures`, `check_policy`)을 싣는다. 새 모듈 `open`(열기 작업 단계)과 `report`(공통 보고 모델: 엔진 `chart_engine`의 `DatasetId`·`DatasetSummary`·`IssueSeverity`·`ValidationIssue`, `pick::FeatureReport`는 다시 내보내기). `ProductLayer`에 `as_any`/`as_any_mut`, `describe_in`, 열기 작업 훅(`open_stage`, `check_opened`, `install_early`, `install`), 보고(`datasets`, `feature_types`, `validation_issues`, `exchange_set_checks`), `check_display`, `numeric_coverages`를 더했다(기본 구현 있음). `platform`에 `UserStore`(`product_state(product, key)`, `product_state_path`), `RecordKind`, `FileRecord`, `user_file_name`을 옮겼다.
- 제품 크레이트:
  - `ferrite-s101`: `layer::open_job`(엔진 `ChartEngine`의 S-101 열기 작업과 설치를 옮김: `S101Stage`(이름 판별, 승인된 계획기·바이트 체인·카탈로그 소유자), `check_opened`(카탈로그 변경 확인), `apply_cancellations`(`install_early`), `install`, `cell_summary`, `exchange_set_checks`), `layer::pick`(엔진 `pick::describe_feature` → `S101Layer::describe_feature`), `layer::pick_fields`(`git mv` 엔진 `dataset_pick_fields.rs`), `layer::resources`(엔진 `scene`의 `CellResourceBinding`, `cell_resource_bindings`, `viewing_groups`, `cell_viewing_groups`). `S101Layer`에서 `portrayal_source`/`precomputed_export`를 없애고 셀 포트레이얼 메서드(`portray_loaded_cells`, `portray_change_cells`, `portray_cells_with_owners`, `portrayal_run`)가 호스트의 `RuleSource`를 인자로 받는다. 셀별 출처 문자열(`provenance`)은 계층이 가진다. 사용자 기록 `publication::{USER_RECORDS, HISTORY_KEY, MEMORY_HISTORY_KEY, history_record, history_path}`.
  - `ferrite-s102`: `layer::open_job`(`git mv` 엔진 `s102_host.rs`: `S102Stage`, 설치와 `"bathymetry"` 보고). 트레이트 `portray`가 호스트 서비스(규칙 출처, 내보내기, 캐시, 상호운용 선택)를 쓰므로 엔진도 트레이트로 부른다. `numeric_coverages`는 수심 커버리지다. 엔진의 S-102 없는 같은 모양 자료형(`S102Options`, `BathymetrySummary`, `OpenedS102`)은 없앴다.
  - `ferrite-s421`: `layer::open_job`(분류 중 경로 캡처, `install_early`에서 삽입과 알림).
  - `ferrite-s124`/`ferrite-s129`: `PRODUCT`(`ProductId`), `panel::PANEL`(패널 서술자), S-124 `user_state::{USER_RECORDS, user_record}`.
- 엔진: `layers::ProductLayers`(등록 순서의 `Box<dyn ProductLayer>`, 역할 `SceneRole`, 형식 접근 `get`/`get_mut`, 단축 `s101()`·`s102()`·`routes()`(기능별), `overlays()`). `EngineState`는 `s101`·`s102` 필드 대신 `layers`를 가지고, 호스트 설정 `portrayal_source`·`precomputed_export`를 가진다. 상호운용(`displayed_products`, `ic_selection`, `ic_level2`)은 계층의 역할·`numeric_coverages`로 구한다. `ChartEngine`의 필드는 상태, 서명 정책, 신뢰 앵커, 삼각분할, 표시뿐이다(`overlays`·`routes`·`provenance`·`s102`는 계층으로). `products::engine_layers`가 S-101(필수 카탈로그), GML 오버레이 계층, S-421, S-102(또는 "not built" 계층) 순서로 등록하고, 뷰어는 `products::chart_layers`(S-101, S-102)를 쓴다. 열기 작업은 입력마다 등록 순서대로 단계의 `sniff`(이름만, 필요하면 앞 64 KiB를 한 번 읽음) → `accept`, 입력 한도(4096) 확인, `after_claims`, `load`를 거치고, 설치는 모든 계층의 `check_opened` → `install_early`(S-101 취소, S-421 경로) → `install`(등록 순서)이다. 보고(`datasets`, `feature_types`, `validation_issues`, `exchange_set_checks`, `feature`)와 장면 합성(바탕 해도 → `SceneRole::Overlay` 명령 → 커버리지 → `SceneRole::Grid` 확장)은 계층 인터페이스로 한다. `OpenReport`는 `products: Vec<ProductReport>`와 `cancellations()`(`s101`)·`bathymetry()`(`s102-pure`)를 가진다. GML 오버레이 계층(`gml_overlay`)은 자기 열기 단계(`GmlStage`: 판별, 서명 스냅숏 읽기, 설치)와 보고를 가진다. 플랫폼의 `NativeUserStore`(`paths: ProductId → 위치`)·`MemoryUserStore`는 등록된 `RecordKind`로 기록을 만든다(NAVWARN 파일 위치·형식과 메모리 키 `navwarn/<user>`, `s101-cancellations` 그대로). 뷰어 전용 열기 계획 `dataset_open_plan.rs`는 앱으로 옮겼다(`git mv`).
- `s101` 기능(기본 켜짐): 끄면 `ferrite-s101`이 등록되지 않고 `PreparedScene::{coverage, catalogue_registry}`, `CellScene`, 셀 포트레이얼 메서드, `cancellation_history`, `set_residency_threshold`, `cell_cache_entries`가 빠진다. 엔진은 `crypto-rust`만, 그리고 각 제품 기능 하나씩으로 빌드된다.
- UI: `ferrite_ui::panels`(`ProductPanel`, `PanelRegistry`, `SharedPanelState`). `overlay_ui::State`는 등록부이고 S-124 `NavwarnPanel`, S-129 `UkcPanel`이 각 UI 기능 뒤에서 등록된다. 공유 옵션(오류 정정, 옵션 변경)은 `shared`에 있다. 보기 메뉴의 패널 항목은 등록부가 그린다(표시 문구 그대로). `AppUiState::overlays` → `panels`.
- 동작: 열기 알림·검증 문제의 순서와 문구, 설치 순서, 포트레이얼 출력(픽셀 회귀 동일), S-102·S-124/S-129·S-421 화면, 캐시 키, NAVWARN 상태 파일 위치·형식, 취소 이력은 그대로다. `ProductLayer` 기본 구현을 직접 부르는 경우의 차이: `S101Layer::open`이 갱신 체인을 받고, 계층들의 `open`은 정책이 서명을 요구할 때 인증되지 않은 입력을 거부한다. `S421Layer::portray`는 예산 초과를 오류로 돌려준다(이유는 `portray_error`에도 남음).
- 테스트 이동: 엔진 `dataset_pick_fields::tests::*` → `ferrite-s101` `layer::pick_fields::tests::*`, 엔진 `dataset_open_plan::tests::*` → 앱 `dataset_open_plan::tests::*`(이름 그대로). 엔진 통합 시험은 계층 접근(`state().layers.s101()`)과 보고 메서드를 쓴다. `overlay_ui::tests::windows_draw_rows_without_raising_requests`는 등록부로 그린다.

### 6단계 결과

- GML 제품 식별(`ferrite_s100_gml::Product` 열거형 제거): `model::GmlIdentity`(`ProductId`, productIdentifier 표기, 제품 코드, 구성원 뒤 식별 정보의 검사 식별자)를 제품 크레이트가 준다(`ferrite_s124::rules::IDENTITY`, `ferrite_s129::rules::IDENTITY`, `GmlRules::identity`; `GmlRules::product()`는 `ProductId`). `Dataset::product`, `OverlayDataset::product()`, `PortrayalInput::product`, `ExchangeSetReport::products`, `namespaced_symbol`은 `ProductId`다. 판독기 `read_file`/`read_bytes(bytes, products, fc, limits)`는 등록된 식별 목록으로 제품을 정하고(productIdentifier, 다음 네임스페이스 `S<code>`, 등록 순서) 검사 식별자 `S-158-129/15`는 S-129 식별이 준다. `overlay::sniff(_head)`, `validate::file_name_follows(&GmlIdentity, ..)`, 교환 세트 검증(`product_of`, 내용 판독, 제품 폴더, productIdentifier 비교)도 등록된 식별을 쓴다. 엔진 `products::gml_products()`는 `Vec<&'static GmlIdentity>`, `GmlOverlays::catalogues`/`attach_user_record`는 `ProductId`를 받는다. 등록된 제품의 판독 결과는 그대로이고, 기능을 끈 빌드에서는 등록되지 않은 GML 제품을 판독하지 않는다.
- Part 9a 변환(`ferrite-part9a`, 새 크레이트): `git mv crates/ferrite-s101/src/lib.rs → crates/ferrite-part9a/src/lib.rs`(변환, 증강 경로·광선 사전 검사, 선·해치 획 해석, `lookup_pc_color`, 시험), `git mv line_geometry.rs`. 변환은 도형 출처 트레이트 `GeometrySource`(피처·점·다중점·곡선·복합 곡선·면 레코드 표)에 대해 일반화했고 `S101Cell`(S-101 셀과 GML 합성 레코드의 공통 모델)이 구현한다. `ferrite-s101/src/lib.rs`는 S-101 고유 모듈 목록과 얕은 수심 무늬 계약(`shallow_pattern_contract`)만 남겼다. 엔진 `s124`/`s129` 기능은 `ferrite-s101`을 연결하지 않는다(`cargo tree`로 확인). 호출자(엔진 오버레이, S-101 셀 포트레이얼, 뷰어, CLI, 예제)는 `ferrite_part9a::*`를 부른다.
- S-421 경로 패널: 데이터 모델 `ferrite_s421::panel`(`RoutePanelData`, `RouteRow`, `WaypointRow`, `RouteAction`, `CatalogueLine`; 제공자 `NATIVE_PROVIDER`·`PLUGIN_PROVIDER`; 게시된 플러그인 UI JSON을 예전 패널과 같은 기본값으로 관대하게 읽음). `ferrite_ui::route_panel::RoutePanel`이 `ProductPanel`로 등록부에 들어간다(`s421` UI 기능, 등록 첫째). `ProductPanel`에 `in_menu`(경로 패널은 보기 메뉴 항목 없음, 도구 모음 단추로 연다)와 `leading`(호스트 도구 창보다 먼저 그림: `PanelRegistry::draw_leading`, 계획기 옆 패널이 화면 공간을 먼저 차지)을 더했다. 호스트는 프레임마다 활성 제공자를 넘기고 패널의 요청(`take_events`)과 창 닫기(`take_toggle`)를 플러그인 이벤트·토글 요청으로 옮긴다(같은 순서). 그리는 위젯과 순서, 문구, 요청 JSON은 그대로다.
- CLI: `compile_error!`를 없애고 `ferrite-s101`을 `s101` 기능의 선택 의존으로 했다. 장면 PC는 `ProductLayer::portrayal_catalogue`(S-101 계층의 PC, GML 오버레이 계층의 첫 설치 PC(설치된 형태를 보관), S-102 계층의 PC)로 정한다. `s101`이 있으면 언제나 S-101 PC라 출력은 그대로다. `palettes`도 같은 PC를 쓴다. `ferrite-cli --no-default-features --features crypto-rust,s124`가 S-124 교환 세트를 검사·렌더링한다(오버레이 기호와 S-124 PC의 DEPDW 배경).
- 제품이 주는 문구: `Provenance::Imported { format, product }`(문구 "not an official {product} dataset", S-124 그대로). 엔진 `catalogue::load_*_catalogue(_from)`는 제품의 `CatalogueLayout`을 받는다(S-101 문구 그대로). 뷰어의 S-102 FC 적재는 S-102 배치를 말한다(예전에는 S-102 FC 폴더가 없을 때도 S-101 배치를 말했다: 유일한 문구 차이).
- 테스트: 이름 그대로 옮김. `ferrite-s101` `{geographic_ray_tests, local_position_adapter_tests, hatch_adapter_tests, hatch_resource_tests}::*`와 `line_geometry::tests::*` → `ferrite-part9a`의 같은 경로. 새 시험: `ferrite-s421` `panel::tests::panel_data_reads_fields_tolerantly_with_the_panel_defaults`. `ferrite-ui` `native_route_ui_cache_tests::native_dto_is_immutable_between_frames_and_replaced_on_publication`은 등록된 경로 패널의 형식 모델로 같은 불변식을 본다. GML 시험은 판독기에 시험용 식별(제품 크레이트의 것과 같은 값)을 넘긴다.

### 남은 일 (2~6단계 참고)

- 뷰어의 S-101·S-102 열기·게시 거래는 엔진 열기 작업(`ChartEngine::open`의 `OpenStage`)을 쓰지 않는다. 뷰어의 열기는 프레임마다 한 항목씩 도는 대기열(경로 → 오버레이 → 래스터 → S-101 묶음)이고 항목마다 거래·되돌리기와 요약 알림을 가진다. 분류는 HDF5 C 판독기의 productSpecification 검사와 S-102 2.1 시험 파일 격리 검사, ZIP 안내 문구(앱 `dataset_open_plan`), S-101 묶음은 카탈로그 변경 거래(`UseInstalled`로 바탕 카탈로그 쌍 교체, 판 혼합 시 카탈로그 소유자 등록부 준비, 활성 색상 프로필·적용된 설정의 Lua 문맥 검사), 배경 로더와 진행률, 레지던시·하이드레이션, 래스터는 GPU 단계 콜백 안의 타일 생성과 게시다. 엔진 열기 작업의 단계(`S101Stage`, `S102Stage`)는 이 중 카탈로그 교체·GPU 게시·대기열 알림을 하지 않으므로 그대로 바꾸면 알림·화면이 달라진다. 설치 단위 조작(`S101Layer::begin_batch`/`insert_loaded`/`finish_batch`, `S102Layer`의 파일 조작)은 이미 엔진 설치와 같은 계층 함수다. 합치려면 `OpenStage`에 호스트 훅(카탈로그 교체 확인, 래스터 게시 콜백, 항목별 알림)을 두고 뷰어 대기열을 그 위에 다시 세워야 한다.
- S-421의 두 저장소(엔진 `S421Layer`, 뷰어 `RouteEditor`)는 그대로다.
- `PreparedScene`의 `coverage`·`catalogue_registry`는 S-101 형식이다(`s101` 기능 뒤). 소비자(CLI 셀 자원, 재투영 프레임의 커버리지 자르기, 웹)가 형식으로 읽으므로 확장으로 옮기면 모든 소비자가 내려받기를 해야 한다. 이번 단계에서는 바꾸지 않았다.
- CLI의 `s101` 없는 장면은 보기 그룹으로 거르지 않는다(표시 모드는 S-101 PC의 것이다). 오버레이 PC에 DEPDW가 없으면(S-129만) 배경은 회색 대체색이다. C ABI는 CLI를 따라 `s101` 없이 빌드된다. 웹은 S-101 캐시·Lua 문맥을 직접 쓰므로 `s101`과 함께 빌드한다.
- S-102 ID(`ProductLayer::remove`의 `u32`)는 계층이 파일 경로마다 매긴다. 뷰어의 데이터셋 트리는 아직 경로(`DatasetLayerId::S102(path)`)로 부른다.
- 렌더러 확장 페이로드는 `Arc<dyn Any + Send + Sync>`라 제품 크레이트가 GPU에 의존하지 않는다. S-102는 GPU로 옮길 때 꺼내야 해서 페이로드 안에 `Mutex`를 둔다(`BathymetryScene::take_tiles`).
- `S101Layer`는 생성 때 플랫폼 서비스 핸들을 복제한다(`Services`). `EngineState::platform`을 나중에 바꾸는 호스트는 계층의 `services`도 함께 바꿔야 한다.
- `ProductLayers`는 `Send`가 아니다(GML 오버레이 계층의 XSLT 상태가 `Rc`를 쓴다). 이전의 `ChartEngine`도 `Send`가 아니었다.

### 이전 단계 참고 (기록)


- `ferrite_s100_gml::Product` 열거형(판독기 제품 판별, `namespaced_symbol`, 교환 세트의 제품 판별)과 판독기의 S-129 검사 식별자(`S-158-129/15`)가 아직 공통 기반에 있다. 5단계에서 `ProductId` 등록으로 바꾼다.
- `Provenance::label`의 가져오기 문구가 S-124를 말한다. 가져오기 제품이 늘면 제품이 문구를 준다.
- `platform::UserStore::navwarn_state`는 S-124 이름이다. 제품별 기록(`product_state(product, user)`)으로 일반화하되 저장 키(`navwarn/<user>`, `FerriteS100/navwarn/<user>.json`)는 유지한다.
- `ferrite-ui::overlay_ui::State`는 S-124·S-129 필드를 고정으로 가진다. 5단계 패널 등록에서 제품별 패널 상태로 나누고 UI 기능 플래그를 둔다.
- `ChartEngine`의 열기 작업(작업 스레드)은 정적 등록(`products::gml_rules`)으로 판별한다. 다른 제품도 정적 규칙과 상태 모듈을 나누면 같은 방식으로 작업 스레드에서 판별할 수 있다.
- 렌더러 확장 페이로드는 `Arc<dyn Any + Send + Sync>`라 제품 크레이트가 GPU에 의존하지 않는다. S-102는 GPU로 옮길 때 꺼내야 해서 페이로드 안에 `Mutex`를 둔다(`BathymetryScene::take_tiles`). S-421은 불변 표본(`NativeRouteWorldSamples`)을 공유하고 페이로드 타입은 API 크레이트(`route_overlay`)에 있다(3단계).
- S-421의 두 저장소: 엔진 쪽 `S421Layer`(캡처한 경로, 읽기 전용)와 뷰어의 `RouteEditor`(컨트롤러가 가진 가져온 경로와 작성 경로). 5단계에서 엔진이 계층을 합성할 때 편집기의 가져온 경로를 계층으로 옮기거나 편집기가 계층을 감싸면 하나로 줄일 수 있다. `S421Layer`의 경로 ID는 현재 리비전의 표시 순서(1부터)라 데이터셋을 닫으면 바뀐다.
- `ChartEngine`의 장면에는 경로 확장이 없다. CLI·웹 렌더러가 경로를 그리려면 `NativeRouteOverlayResources`(편집 PC, 팔레트)를 플랫폼 소스에서 적재하고 `SceneEmitter`가 `from_extension` → `PreparedNativeRouteGpuPublication`을 거쳐야 한다(현재 `sync_native_route_gpu`는 뷰어 렌더러 전용 경로).
- 경로 패널(`ferrite-ui::egui_integration`)은 여전히 플러그인 UI JSON(`ferrite_s421::ui::UiData` + 편집기 필드)을 그린다. `S421::panels()`가 패널(`S-421/routes`)을 알리지만 형식이 있는 패널 모델로 바꾸는 일은 5단계 패널 등록과 함께 한다.
- `ferrite-s421`의 플러그인 계층(`RoutePlugin`, `Plugin` 구현)과 외부 `plugins/route-plugin`은 그대로 컨트롤러를 공유한다. 뷰어는 정적으로 연결한 같은 컨트롤러를 쓴다.
- `ProductLayer::portray`는 실패를 돌려줄 수 없고(`Option`) 요청에 규칙 출처·상호운용 합성이 없다. S-102는 트레이트 구현에서 계층 옵션·캐시와 표시 설정(`PortrayalRequest::settings`, 2단계에서 추가)만 쓰고 실패는 기록 후 None으로 처리한다. 엔진은 오류를 전파해야 하므로 `S102Layer::prepare`를 직접 부른다. 5단계에서 `portray`를 `Result`로 바꾸고 요청에 호스트 서비스(규칙 출처, 캐시, 상호운용 선택)를 넣으면 엔진도 트레이트만 쓸 수 있다.
- `ProductLayer::open`(바이트 입력)은 서명 스냅숏을 보관하지 않는다. 엔진의 열기 작업은 서명 정책·데이터 원천 포착이 필요해 `open_inputs`/`install`을 쓴다. 단계 5에서 `DatasetInput`에 인증 스냅숏을 싣는 방안을 검토한다.
- S-102 ID(`ProductLayer::remove`의 `u32`)는 계층이 파일 경로마다 매긴다. 뷰어의 데이터셋 트리는 아직 경로(`DatasetLayerId::S102(path)`)로 부른다.
- 뷰어의 래스터 경로(`load_bathymetry_transaction`, 장면 재구성의 `stage_s102_raster_batch`, `audit_bathymetry`)는 같은 모자이크·타일 계산을 앱에서 반복한다. GPU 단계 콜백 안에서 타일을 만드는 구조라 `S102Layer::prepare`로 바로 바꾸지 못했다. 타일을 하나씩 내주는 반복자(`prepare`의 내부)를 계층이 공개하면 합칠 수 있다.
- 엔진은 S-102 입력을 확장자(`s102_host::is_hdf5_input`)로 고른다. `S102::sniff`(HDF5 서명)로 바꾸려면 열기 작업이 앞부분을 읽어야 한다(5단계 정적 판별과 함께).
- `s102-pure`가 꺼진 엔진은 `s102_host`의 같은 모양 자료형(`S102Options`, `BathymetrySummary`)을 따로 정의한다. 5단계에서 제품 보고를 공통 모델로 옮기면 없앨 수 있다.
- S-101 기능 플래그: 계층 합성(아래)으로 `EngineState::s101`이 선택 항목이 되면 `s101` 기능을 둔다. 현재 엔진이 S-101을 직접 부르는 곳은 `state`(얇은 포트레이얼 메서드), `chart_engine`(열기 작업의 계획·바이트 체인·카탈로그 소유자, 설치, 보고), `scene`(리소스 바인딩·보기 그룹), `pick::describe_feature`, `dataset_open_plan`(체인 파일 판별), `dataset_discovery`(재귀 탐색 다시 내보내기), `platform`(`HistoryFile`)이다.
- `S101Layer`는 생성 때 플랫폼 서비스 핸들을 복제한다(`Services`). `EngineState::platform`을 나중에 바꾸는 호스트는 없지만, 바꾸려면 계층의 `services`도 함께 바꿔야 한다.
- `portrayal_source`/`precomputed_export`는 S-101 계층에 있고 엔진 `s102_host`도 읽는다(호스트 설정). 5단계에서 `PortrayalRequest`(호스트 서비스)로 옮긴다.
- `ProductLayer for S101Layer`의 `open`은 기본 셀 하나의 바이트만 받는다(갱신 체인, 서명 스냅숏, 생명주기 메타데이터, 취소는 계획기와 호스트 설치). `portray`는 레지던시·셀 캐시·상호운용 없이 전체를 그린다. `describe`/`highlight`는 비어 있다(S-101 피킹은 엔진 `pick`이 그린 명령의 셀 색인으로 푼다). 5단계 피킹 공통화에서 `FeatureReport`·`dataset_pick_fields`를 계층으로 옮기면 `describe`를 채울 수 있다.
- `ChartEngine`의 열기 작업(`load_planned_charts`, `inventory_owners`, `load_byte_charts`, `plan_byte_chains`)과 뷰어의 열기·게시 거래(`main.rs`의 로더, 카탈로그 변경 거래)는 아직 따로 S-101 계획기를 부른다. 계층의 열기 작업(`open_inputs` → `install`, S-102와 같은 모양)으로 모으면 엔진의 S-101 분기를 없앨 수 있다.
- 셀별 출처 문자열(`ChartEngine::provenance`)은 파사드에 있다. 계층 열기로 모을 때 계층으로 옮긴다.
- 엔진 `catalogue`의 적재 오류 문구는 S-101 배치(`Catalogues/FC/S-101`)를 말한다. 제품별 카탈로그 적재로 나눌 때 제품이 문구를 준다.
