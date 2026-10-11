use ferrite_kernel::coverage_frame::CoverageFrame;
use ferrite_kernel::coverage_selection::{select_coverages, CoverageFootprint, Region};
use ferrite_kernel::scale_policy::CoverageScaleRange;
fn rect(x: f64, width: f64) -> Region {
    Region::from_rings(
        &[
            [x, 0.],
            [x + width, 0.],
            [x + width, 10.],
            [x, 10.],
            [x, 0.],
        ],
        &[],
    )
    .unwrap()
}
fn coverage(
    dataset: usize,
    id: i64,
    x: f64,
    width: f64,
    min: u32,
    opt: u32,
    max: u32,
) -> CoverageFootprint {
    CoverageFootprint {
        dataset_id: dataset,
        coverage_id: id,
        region: rect(x, width),
        scales: CoverageScaleRange {
            minimum_denominator: Some(min),
            optimum_denominator: opt,
            maximum_denominator: max,
        },
    }
}
#[test]
fn larger_scale_portion_has_indication_but_no_overscale_fill_at_deep_zoom() {
    let inventory = [
        coverage(0, 10, 0., 5., 25_000, 12_500, 8_000),
        coverage(1, 20, 5., 5., 100_000, 50_000, 50_000),
    ];
    let viewport = rect(0., 10.);
    let selected = select_coverages(&inventory, 3500., &viewport).unwrap();
    assert!(selected.uncovered.is_empty());
    assert_eq!(selected.coverages.len(), 2);
    let frame = CoverageFrame::new_with_scale_annotations(
        &inventory,
        &selected,
        &viewport,
        [10, 10],
        4096,
        3500.,
        [2., 2.],
    )
    .unwrap();
    let annotations = frame.scale_annotations().unwrap();
    let finer = annotations.reference().unwrap();
    assert_eq!(finer.dataset_id, 0);
    assert!(finer.state.indicator_required);
    assert_eq!(finer.state.factor, 12_500. / 3500.);
    let pattern = annotations.overscale_pattern_mask(4096).unwrap().unwrap();
    assert!(
        pattern.contains_pixel(7, 2),
        "coarser gap portion needs pattern"
    );
    assert!(
        !pattern.contains_pixel(2, 2),
        "S98 12.3.3 larger-scale portion needs indication only"
    );
}
#[test]
fn strict_maximum_endpoint_and_initial_band_gap_are_separate_rights() {
    let s = CoverageScaleRange {
        minimum_denominator: Some(90_000),
        optimum_denominator: 45_000,
        maximum_denominator: 12_000,
    };
    assert!(!s.overscale(12_000., true).unwrap().pattern_required);
    assert!(s.overscale(11_999., true).unwrap().pattern_required);
    assert!(!s.overscale(11_999., false).unwrap().pattern_required);
}

#[test]
fn offscreen_finer_inventory_does_not_make_primary_area_a_gap_fill() {
    let inventory = [
        coverage(0, 10, 20., 5., 25_000, 12_500, 8_000),
        coverage(1, 20, 0., 10., 100_000, 50_000, 50_000),
    ];
    let viewport = rect(0., 10.);
    let selected = select_coverages(&inventory, 3500., &viewport).unwrap();
    assert_eq!(selected.coverages.len(), 1);
    assert!(selected.coverages[0].selected_to_fill_gap);
    let frame = CoverageFrame::new_with_scale_annotations(
        &inventory,
        &selected,
        &viewport,
        [10, 10],
        4096,
        3500.,
        [2., 2.],
    )
    .unwrap();
    let a = frame.scale_annotations().unwrap();
    assert!(a.reference().unwrap().state.indicator_required);
    assert!(a.overscale_pattern_mask(4096).unwrap().is_none());
    assert!(a
        .overscale_pattern_masks_by_dataset(4096)
        .unwrap()
        .is_empty());
}
