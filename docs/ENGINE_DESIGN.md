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
| 6 | **완료.** `ferrite-wasm`(wasm-bindgen, `crates/ferrite-wasm`)과 웹 데모(`crates/ferrite-wasm/www`, 빌드 `tools/build_wasm.py`). `cargo check -p ferrite-engine --target wasm32-unknown-unknown --no-default-features --features crypto-rust`가 통과하고 `ferrite-gpu`·`ferrite-cli` 라이브러리도 wasm32로 빌드된다. **wasm 차단 요소와 대응**(네이티브 동작은 그대로): `getrandom` 0.3(regexml→ahash)은 wasm32 대상에서만 `wasm_js` 기능(`ferrite-portrayal-catalog`), `std::time::Instant::now`(wasm에서 패닉)는 `ferrite_kernel::time::Instant`(네이티브는 `std::time::Instant` 그대로, wasm은 `web-time` =1.1.0)로 엔진·렌더·GPU의 타이밍 로그와 NAVWARN 재검사 시각을 바꿈, `SystemClock`은 wasm에서 chrono(JS `Date`)로 시각을 읽고 웹 바인딩은 `Date.now()` 기반 `JsClock`을 주입, `ferrite-s421`(플러그인 계층 `abi_stable`/`libloading`이 wasm 불가)은 엔진에서 wasm이 아닌 대상에만 의존하고 웹은 `s421_dataset_input`의 `Profile`·`MAX_XML_BYTES` 사본을 씀(일치 테스트 포함), `wgpu`는 wasm 대상에서 `webgl`·`fragile-send-sync-non-atomic-wasm` 기능, `GpuState`는 웹에서 WebGPU 감지 후 없으면 WebGL2(다운레벨 한도)로 인스턴스를 만듦, wasm의 `std::fs::File`에는 `Drop`이 없어 네이티브 전용 원자 쓰기 경로 세 곳의 `drop(file)`에 `clippy::drop_non_drop` 허용. rayon(`ferrite-gpu` 직접 의존과 `ferrite-render/parallel`)은 wasm에서 rayon-core가 호출 스레드 하나로 대체한다. **API** `WebEngine`: `new([[이름, 바이트]...])`/`from_bundle(catalogues.bin)`(FC/PC S-101 필수, S-124·S-129 선택), `add_file(path, bytes)`, `open()`→보고서 JSON, `report()`, `palettes()`, `set_palette`, `load_precomputed(name, bytes)`, `prepare(w, h, density, lat, lon, scale_or_zoom, is_scale)`→장면 요약 JSON, `pick_json(x, y, r)`, `feature_json(cell_index, feature_id)`, `render_to_canvas(WebCanvas)`. `WebCanvas.create(canvas_id, density, backend?)`(async)는 캔버스 서피스에 `ferrite-gpu`를 붙이고 `adapter()`로 어댑터와 백엔드를 알려 준다. `backend`는 `"auto"`(기본: WebGPU가 있으면 WebGPU, 없으면 WebGL2), `"webgpu"`, `"webgl2"`이고 `WgpuRenderer::new_with_backends`/`GpuState::new_with_backends`로 wgpu 인스턴스 백엔드(`BROWSER_WEBGPU`, `GL`)를 제한한다(네이티브 `new`는 그대로 `Backends::all()`). 엔진은 `Platform::in_memory` + `SequentialExecutor` + `MemoryCacheStore`이고 S-101은 항상 `PortrayalSource::Precomputed`이다. 렌더러 방출은 CLI와 같은 `ferrite_cli::SceneEmitter`(이번에 `OffscreenChartRenderer`에서 분리)이다. 내장 브라우저(WebGPU, Apple M5)에서 SHOM 교환 세트(셀 4개, 사전 계산 페이로드)·UKHO S-124 5개·IHO S-129를 열어 같은 위치의 CLI 렌더와 같은 명령 수(70114)·모습으로 그리고, 팬·줌·피킹·Night 팔레트를 확인했다. **WebGL2**: 데모의 `?backend=webgl2`(또는 백엔드 선택)로 같은 브라우저에서 WebGL2(ANGLE Metal)를 강제해 같은 샘플을 그려 보니 수정 없이 동작했다. 명령 수(70114)·축척이 WebGPU와 같고, Day 개요·줌 2.7·줌 20과 Night 줌 2.7 스크린숏이 WebGPU와 픽셀 단위로 사실상 같았으며(JPEG 잡음 수준), 팬·줌·피킹·리사이즈·팔레트 전환에서 콘솔 오류가 없었다. 렌더러의 기본 경로는 저장 버퍼·컴퓨트를 쓰지 않는다(유일한 컴퓨트 경로 `retained_world_area`는 `FERRITE_RETAINED_WORLD_AREAS` 옵트인이고, 켜더라도 다운레벨 한도의 저장 버퍼 크기 0에서 기존 한도 검사로 사양된다). 타임스탬프 쿼리는 어댑터가 제공할 때만 요청한다. **남은 것**: S-102, 웹 워커·OffscreenCanvas, IndexedDB 캐시. Lua 포트레이얼과 증분 렌더는 6b단계 |
| 6a | **완료. 남은 네이티브 전용 경로의 플랫폼 서비스 전환.** 네이티브 동작(같은 파일·순서·스냅숏·오류)은 그대로이고 경로 함수는 `NativeDataSource`를 쓰는 `_in` 함수의 래퍼다. **S-101 서명 검증·업데이트 계획**: `s101_update_plan`의 `expand_candidates_in`·`authorized_folder_plans_in`·`authorized_batch_in`·`authorized_plans_in`(test-support)과 새 `authorized_selection_plans_in`이 `DataSource`로 읽는다. 로컬 소스는 예전처럼 비공개 스냅숏 파일(인증 스냅숏 파일, `UnauthenticatedSnapshot::copy_bounded`)을, 그 밖의 소스는 소유 바이트(인증 스냅숏의 메모리 바이트, 같은 상한의 사본)를 보관하고 `Plan::load`는 `S101Cell::load_update_chain_bytes_with_identity`로 같은 한도·검증·원본 식별자를 낸다. `s101_lifecycle_metadata::capture_cached_in`과 `MetadataEvidence::verify_retained`(`RetainedInput::{File, Bytes}`)는 비검증 `CATALOG.XML`도 소스로 읽고 `dataset_signature_policy::unchecked_datasets_in`은 소스의 정규 경로를 키로 쓴다. `s101_xc_*`·`cell_source_identity`·`chart_publication`의 생명주기 검사는 원래부터 보관된 바이트만 다룬다. `ChartEngine`은 로컬 소스이거나 `SignaturePolicy::Required`이면 이 계획기로 열고(메모리 경로는 권한 키이므로 절대 경로), `Off`인 비로컬 소스는 기존 바이트 계획을 유지한다(웹의 상대 경로 이름). 폴더에서 찾은 S-101 입력은 정규 경로로 바꿔 계획한다(이전에는 `..`가 든 폴더 경로에서 생명주기 메타데이터 키가 어긋나 셀이 거부됐다). **취소 이력**: `UserStore::cancellation_history`(`UserRecord`, 새 `UserRecord::lock`)로 옮겼다. 네이티브는 `chart_publication::HistoryFile`(같은 파일, `.lock` OS 잠금, `create_new` 임시 파일과 rename), `MemoryUserStore`는 `s101-cancellations` 레코드(`MEMORY_HISTORY_KEY`)다. `HistoryStore::begin_record`·`CancellationHistory::read_record`를 추가했고 앱이 쓰는 경로 함수(`begin`, `read`)는 그대로다. 파사드의 `DatasetSource::Files`/`Bytes`는 명시 선택이므로 뷰어처럼 선택한 갱신의 기본·선행 파일을 함께 열고(`expand_candidates_in`), 선택된 물리 취소를 로드된 대상(또는 대상이 없으면 공지)과 이력으로 검증해 적용한 뒤 이력에 기록한다(`OpenReport::cancellations`, `ChartEngine::cancellation_history`). 폴더에서 발견한 취소는 적용하지 않고, 설치되는 데이터셋은 이력의 이름 재사용 규칙(`validate_reuse`)을 지켜야 한다. **설치 카탈로그 인벤토리**: `resolve_pair_exact_first_in`·`DatasetCatalogueRegistry::stage_in`, `EngineState`와 파사드는 플랫폼 소스로 인벤토리를 읽는다. **S-421 입력**: `s421_dataset_input::capture_if_route_in`·`discover_routes_in`. 파사드는 S-421 경로를 캡처해 `ChartEngine::routes()`로 내준다(포트레이얼 없음, `Required`에서는 맨 GML 경로를 받지 않음). 테스트 `tests/in_memory_exchange_sets.rs`: SHOM 2026년 5월 교환 세트 전체와 ed1er2(기본+갱신 2개)를 메모리와 네이티브에서 `Required`(고정 시계)로 열어 데이터셋·판·갱신·원본 식별자·교환 세트 검사가 같은지, 변조되거나 빠진 갱신의 거부, 서명된 합성 취소(독립 설치 루트)의 폴더 비적용·명시 적용·`MemoryUserStore` 이력·이름 재사용 거부·다른 엔진에서의 재생, 메모리 S-421 경로. **네이티브 전용으로 남은 것**: S-102 전체(별도 작업), `ferrite-s421`의 플러그인 계층과 경로 포트레이얼(앱의 `native_s421`), 앱의 열기 대화 계획(`dataset_open_plan`), S-124/S-129 shapefile 가져오기, `ferrite_security::capture_catalogue_authenticated_incoming_exchange`(수신 폴더 캡처), 파일 기반 캐시 구현(`NativeCacheStore`·`secure_instruction_cache`), PC 캡처의 병렬 읽기, `ferrite-gpu` 내부 rayon |
| 6b | **완료. 웹 Lua 포트레이얼과 증분 렌더.** **Lua 방식 선택**: (a) Lua C 소스를 wasm32-unknown-unknown으로 컴파일, (b) 순수 Rust Lua 구현, (c) 웹 빌드 전체를 wasm32-unknown-emscripten으로 전환을 검토해 (a)를 택했다. (c)는 wasm-bindgen이 emscripten 대상을 지원하지 않아 웹 바인딩과 wgpu 경로를 새로 짜야 하고, (b)는 S-101 규칙 전체(문자열 서식, 패턴, 코루틴, pcall)를 C Lua와 바이트 단위로 같게 돌린다고 볼 만큼 성숙한 크레이트가 없다. (a)는 WebAssembly 대상이 있는 clang(Homebrew LLVM 23에서 확인)과 `llvm-ar`만 있으면 되고 네이티브와 같은 인터프리터 소스를 쓴다. **구현**: `ferrite-lua-runtime`의 mlua 의존을 대상별로 나눴다. 네이티브는 그대로 `vendored`(lua-src 550.0.0, Lua 5.4.8)이고, wasm32-unknown-unknown은 mlua-sys `external`(=0.10.0)이며 빌드 스크립트가 같은 lua-src 소스를 lua-src의 WASI 설정(`LUA_USE_POSIX`+WASI 가드, `LUA_COMPAT_5_3`, `-mllvm -wasm-enable-sjlj` 새 예외 처리(exnref))으로 컴파일해 링크한다(`FERRITE_WASM_CC`·`FERRITE_WASM_AR`로 도구 지정, 없으면 Homebrew LLVM 경로 또는 PATH). 시스템 헤더 대신 `wasm-libc/include`의 최소 헤더를 쓰고, `wasm-libc/ferrite_libc.c`가 문자열·ctype·`vsnprintf`·스트림(파일 없음, stdout/stderr 버림)·`__wasm_setjmp`/`__wasm_longjmp`와 `__c_longjmp` 태그·`frexp`를, `src/wasm_libc.rs`가 힙(`malloc` 등)과 부동소수 변환(`%e %f %g %a`, `strtod`)을 Rust의 정확 반올림 서식·파서로 제공한다. 수학 함수와 `mem*`는 Rust compiler-builtins를 쓴다. 서식·파서는 네이티브에서 호스트 C 라이브러리와 비교하는 테스트(값 약 1만 2천 개 × 서식 20개, 파싱 입력 약 1만 6천 개)로 확인했다. Apple libc와 다른 두 경우(`%g`가 정확한 십진 동률을 0으로 내림 반올림할 때 끝 0을 남김, `%.Na` 절삭)는 C99·glibc 동작을 따른다. `table.sort` 피벗 난수(`arc4random`)는 고정값이다. wasm-bindgen 0.2.108(walrus 0.24)이 예외 처리 명령을 그대로 통과시키고, Lua `error`/`pcall`은 Rust 프레임을 넘는 wasm 예외로 동작한다. `ferrite-lua`의 시간 측정은 `ferrite_kernel::time::Instant`로 바꿨다(wasm의 `std::time::Instant`는 패닉). 웹 기능은 `ferrite-wasm/lua-wasm`(엔진 `lua54`)이고 `tools/build_wasm.py`는 기본으로 켠다(`--no-lua`로 끔). 네이티브 기본 빌드는 바뀌지 않는다. **동일성**: `tools/lua_wasm_parity.py`가 같은 `WebEngineCore`로 네이티브(`examples/part9a_dump.rs`, mlua)와 wasm(Node, `tools/lua_wasm_parity.mjs`)에서 데이터셋 묶음마다 `HostPortrayalEmit` 호출(피처 참조, Part 9a 그리기 명령 문자열, 관찰 문맥 매개변수)을 순서대로 기록해 비교한다(`PortrayalEngine::process_owned_cell_recording`). TestData의 SHOM·UKHO·IHO S-101 58개 묶음(내용이 같은 묶음은 한 번, 셀 61개, Part 9a 출력 91,954개)이 모두 바이트 단위로 같고, 네이티브를 두 번 돌린 결과도 같다. 관찰 매개변수 목록은 규칙이 Lua 테이블 순회 순서로 만들기 때문에 해시 시드(네이티브도 주소·시각 기반)에 따라 순서가 달라지므로 정렬해 비교한다(엔진도 정렬해 쓴다). 그리기 명령 문자열은 정렬 없이 같다. UKHO S-101 시험 셀 17개와 IHO 시험 셀 9개는 사양 1판이라 두 빌드 모두 같은 오류로 열지 않는다. **포트레이얼 선택**: `WebEngine.set_portrayal("auto"|"lua"|"precomputed")`, `portrayal()`, `WebEngine.lua_available()`. `auto`는 Lua가 빌드되어 있고 사전 계산 페이로드를 넣지 않았으면 Lua를 쓰고, 페이로드를 넣었으면 페이로드를 쓰되 빠진 셀이 있으면 그 준비를 Lua로 다시 한다. 장면 요약의 `portrayal`과 데모 HUD·S-101 선택 상자가 사용한 소스를 보여 준다. 웹 Lua도 메모리 캐시 저장소의 셀 명령 캐시를 써서 첫 준비 뒤의 재포트레이얼은 캐시를 읽는다. **증분 렌더**: `ChartEngine::reproject(scene, view)`가 뷰어의 팬·줌 경로(`rebuild_view_ex`: 카메라·뷰포트·픽셀 비율, 새 뷰의 평면 커버리지)만 수행하고, 새 뷰의 레지던시(`residency_for_view`)가 장면의 레지던트 셀과 다르면 false를 돌려 `prepare`가 다시 포트레이얼하게 한다. `WebEngine.set_view(...)`(인자는 `prepare`와 같음)는 데이터셋·표시·포트레이얼 선택이 그대로면 재투영하고 아니면 준비하며, 요약의 `reprojected`로 알려 준다. 재투영한 장면은 같은 뷰를 새로 준비한 장면과 명령 수·축척이 같고 오프스크린 렌더 픽셀이 같다(`ferrite-wasm` 테스트, S-124와 Lua S-101). GPU 쪽은 기존 방출 경로(`SceneEmitter`)를 그대로 쓴다. **브라우저 측정**(내장 브라우저, Apple M5, 1920×1600 물리 픽셀, 사전 계산 페이로드 없음): SHOM 2026년 3월 교환 세트(셀 4개, 69,570 명령)는 Lua 첫 준비 2.4 s(WebGL2 4.5 s), 팬·줌 한 번에 전체 `prepare` 200~220 ms(셀 캐시 적중)였던 것이 `set_view` 2~6 ms, 그리기 12~20 ms(첫 프레임 뒤)가 됐다. UKHO S-101 교환 세트(셀 27개, 247,414 명령)는 Lua 첫 준비 7.2 s, 전체 `prepare` 1,054 ms → `set_view` 10~20 ms(한 번 132 ms), 그리기 37~570 ms. 같은 위치의 CLI 렌더와 명령 수·모습을 비교했다(아래). **남은 것**: 웹 워커·OffscreenCanvas(첫 Lua 포트레이얼이 UI 스레드를 수 초 막음), IndexedDB 캐시(셀 명령 캐시는 페이지 수명 동안만), GPU 쪽 증분(재투영 뒤에도 모든 명령을 다시 방출하므로 큰 장면은 그리기가 수백 ms), 셀 단위 사전 계산·Lua 혼합(`auto`는 준비 단위로 전환), Lua 5.5 wasm 빌드 검증, 레지던시가 켜지는 큰 셀 묶음의 브라우저 측정(시험 데이터는 임계값 아래) |
| 6c | **완료. S-102를 바이트와 순수 Rust HDF5로.** **판독기 설계**: `ferrite-s102::h5`가 S-102 어댑터가 쓰는 HDF5 부분(그룹·링크·속성·데이터셋, `hdf5` 크레이트와 같은 이름의 `TypeDescriptor`, `read_raw`·`read_slice_2d`·이름 기준 복합 멤버 읽기 `read_field`·`read_f32_fields_2d`, `H5Dvlen_get_buf_size`와 같은 가변 길이 예측)을 백엔드 중립 API로 감싼다. 백엔드는 `hdf5-metno`(새 `hdf5` 기능, 기본, 로컬 경로의 기본 판독기)와 `h5::pure`(외부 크레이트 없이 새로 작성, 항상 빌드)이다. 검토 시점에 성숙하고 고정 가능한 순수 Rust HDF5 판독 크레이트가 없어 S-102에 필요한 범위만 구현했다: 수퍼블록 v0~v3, 객체 헤더 v1/v2(연속 블록, lookup3 체크섬 검증), 옛 그룹(심볼 테이블, v1 B-트리, 로컬 힙)과 새 그룹(링크 메시지, 프랙털 힙+v2 B-트리 밀집 링크), 압축·밀집 속성, 정수·IEEE 실수·고정/가변 문자열·복합·열거·배열 형식(빅엔디언 포함), 압축·연속·청크 레이아웃(v1 B-트리, 단일 청크, 암시적, 고정 배열, v2 B-트리 색인), deflate(flate2 =1.1.8)·shuffle·Fletcher-32 필터, 채움 값. 공유 메시지·커밋된 형식·외부/가상 저장소·szip·n-bit·scale-offset·확장 배열 색인·거대 힙 객체·필터 힙·소프트/외부 링크 대상은 오류로 거부한다(테스트 포함). 형식 변환은 C 라이브러리를 따른다(정수 범위 클리핑, 열거→정수, 고정 문자열 패딩 규칙 `H5T__conv_s_s`, 가변 문자열의 C 문자열 의미, `H5T__set_loc`의 메모리 배치 기술자). 경로용 판독기는 `FERRITE_S102_HDF5_READER=pure`로 바꿀 수 있다. 기존 크레이트 API(`singleton`, `pub use hdf5`)는 엔진의 `dataset_open_plan`을 위해 남겼다. **동일성**: `tests/reader_parity.rs`가 TestData의 S-102 파일 89개(내용이 다른 50개: UKHO 시험 셀 2.1, UKHO 2026 3.0, SHOM 3.0, 모두 수퍼블록 v0·청크 deflate)와 C 라이브러리로 만든 합성 파일(기본·earliest·latest 형식, 청크·deflate·shuffle·Fletcher-32, 밀집 속성·링크, 고정 배열·v2 B-트리·단일 청크 색인, 채움 값, 열거·불리언·고정/가변 문자열·빅엔디언)을 두 판독기로 읽어 구조·속성 값·데이터셋 값·가변 길이 예측과 S-102 해석(메타데이터, 전체 격자 샘플 비트, 품질 레코드·ID, 시험 셀 창)을 비교한다. 모두 같다. 메모리 이미지도 파일과 같다. 크레이트 단위 테스트(138개)는 `FERRITE_S102_HDF5_READER=pure`에서도 모두 통과한다. 2501×2502 격자(UKHO 2026, 6.3백만 샘플) 전체 창 읽기는 release에서 C 약 0.2 s, 순수 Rust 약 0.33 s(deflate가 miniz_oxide)이다. **엔진**: 기능 `s102-pure`(C 없음, `cargo check -p ferrite-engine --target wasm32-unknown-unknown --no-default-features --features crypto-rust,s102-pure` 통과)와 `s102-hdf5`(= `s102-pure` + C 판독기 + Lua). `ferrite-s102`는 `lua-native` 없이도 빌드되며 그때 포트레이얼은 규칙이 낸 Part 9a 명령 문자열(`BathymetryPortrayal::rule_instructions`/`from_bound_instructions`)로 만든다. `s102_input_capture::CapturedInput::capture_in`(데이터 소스 또는 인증 메모리 스냅숏의 바이트, 같은 512 MiB 상한, `verify`는 바이트 해시), `s102_depth_policy::DepthPolicy::from_bytes`, `s102_trial_worker::inspect_bytes`(프로세스 격리 없이 순수 판독기), `s102_journal_admission`의 메모리 스냅숏 검증. 파일 경로 함수와 앱 동작은 그대로다. **파사드**: `ChartEngine::new`가 `PC/S-102`를 데이터 소스로 읽고(선택), `open`은 `.h5`/`.hdf5` 입력을 서명 정책(`authorize`)과 캡처를 거쳐 메모리 바이트를 순수 판독기로 해석한다(`productSpecification` 3.0.0은 커버리지, 2.1은 검사 전용 시험 셀, 그 밖은 문제). `install`은 뷰어처럼 `EngineState::bathymetry`·`depth_policies`·`bathymetry_bounds`·`s102_trials`에 넣고 `OpenReport::bathymetry`(`BathymetrySummary`)로 알린다. S-102 PC가 없으면 커버리지는 문제로 거부한다. `prepare`는 `PreparedScene::bathymetry`(`BathymetryScene`: 표시 모드의 뷰잉 그룹, 파일마다 원본 또는 보수적 데이텀 모자이크를 `tile_edge`(기본 2048) 창으로 나눈 `BathymetryTile::Bound`(PC 평가 봉인 재질) 또는 IC 합성 `Composed`)을 만든다. 데이터 범위에 래스터 범위를 더한다. Lua가 없으면 캐시 저장소의 사전 계산 규칙 명령(`set_precomputed_export`로 Lua 빌드가 기록, SHA-256 무결성)을 쓰고 없으면 명확한 오류다. API: `ChartEngine::bathymetry()`, `s102_options()`/`set_s102_options(S102Options { depth_adjustments, tile_edge })`, `s102_scene::SUPPORT`. 테스트 `tests/in_memory_s102.rs`: 메모리 소스의 SHOM 3.0 두 개와 UKHO 2.1 시험 셀을 열고 준비, Lua 타일과 사전 계산 타일의 픽셀 동일, 앱 파이프라인(C 판독기, 파일 깊이 정책, Lua 규칙)과 타일 픽셀 동일, PC 없음·손상 바이트의 문제 보고. **남은 것**: 장면의 S-102 타일을 GPU로 그리는 호스트 방출(`ferrite_cli::SceneEmitter`·웹 바인딩), EPSG:4326 외 좌표계와 다각형 도메인의 포트레이얼(뷰어와 같이 거부), 품질 속성 해석의 프로세스 격리(파사드는 프로세스 안에서 해석), 파사드의 S-102 취소 저널 게이트(앱만 사용), 확장 배열 청크 색인 등 위 거부 목록 |

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

`build_wasm.py`는 `cargo build -p ferrite-wasm --target wasm32-unknown-unknown`, `wasm-bindgen --target web`,
`www/` 복사, 카탈로그 묶음 `catalogues.bin`(S-101·S-124·S-129 FC/PC) 작성을 하고 결과는 저장소 밖에 둔다.
데모는 파일 놓기(S-124/S-129 GML, S-101 셀, `.ferrite_pre_*` 페이로드)와 `--sample` 묶음 불러오기,
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
