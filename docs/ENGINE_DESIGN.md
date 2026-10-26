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
| 3 | `ferrite-wgpu`를 `ferrite-gpu`(렌더러)와 `ferrite-ui`(egui)로 분리, 렌더러는 `PreparedScene`만 입력 | 중간 |
| 4 | `ChartEngine` 파사드, 문서, 예제(`headless_render`, `pick`), 기능 플래그(`lua54`/`lua55`, `s102`, `gml`) | 낮음 |
| 5 (선택) | C ABI(`ferrite-engine-capi`), S-57/S-52 어댑터 | 별도 프로젝트 규모 |

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
