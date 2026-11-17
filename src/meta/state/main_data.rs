#[derive(Debug, PartialEq)]
pub enum MainDataSignal {
    MiddleVoltage(f64),             // "V1/V12_MidleVoltage"
    MainQ(f64),                     // "TotalMidleKVar"
    MainP(f64),                     // "TotalMiddlePower"
    CondensatorsRelayState(f64),    // "Condensators_reley_output"
    SubStationOneState(f64),        // "462_condensators_state"
    GenCosSetpointOutput(f64),      // "Cos_phi_engine_output"
    GenActivePower(f64),            // "KGU_gen_power"
    GenSetterLocalRemoteState(f64), // "Distance_working_reguler_engine"
    GenEngineRun(f64),              // "KGU_generator_work"
}

impl MainDataSignal {
    /// Tags currently connected to the control loop. Auxiliary feedback is not wired yet.
    pub const TAGS: &'static [&'static str] = &[
        "V1/V12_MidleVoltage",
        "TotalMidleKVar",
        "TotalMiddlePower",
        "Condensators_reley_output",
        "Cos_phi_engine_output",
        "KGU_gen_power",
        "Distance_working_reguler_engine",
        "KGU_generator_work",
    ];

    pub fn from_tag(tag: &str, value: f64) -> Option<Self> {
        match tag {
            "V1/V12_MidleVoltage" => Some(Self::MiddleVoltage(value)),
            "TotalMidleKVar" => Some(Self::MainQ(value)),
            "TotalMiddlePower" => Some(Self::MainP(value)),
            "Condensators_reley_output" => Some(Self::CondensatorsRelayState(value)),
            "Cos_phi_engine_output" => Some(Self::GenCosSetpointOutput(value)),
            "KGU_gen_power" => Some(Self::GenActivePower(value)),
            "Distance_working_reguler_engine" => Some(Self::GenSetterLocalRemoteState(value)),
            "KGU_generator_work" => Some(Self::GenEngineRun(value)),
            _ => None,
        }
    }
}
