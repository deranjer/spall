use super::target_to_striker_normal;

#[test]
fn contact_pair_order_orients_sampling_into_either_target() {
    let body_zero_to_one = [1.0, 0.0, 0.0];
    // Target in slot 0: the struck material lies in +X, toward striker 1.
    assert_eq!(
        target_to_striker_normal(body_zero_to_one, 0),
        body_zero_to_one
    );
    // Target in slot 1: the struck material lies in -X, toward striker 0.
    assert_eq!(
        target_to_striker_normal(body_zero_to_one, 1),
        [-1.0, 0.0, 0.0]
    );
}
