pub enum MainDataSignal {
    MiddleVoltage(f64), // "V1/V12_MidleVoltage"
    MainQ(f64),       // "TotalMidleKVar"
    MainP(f64),                // "TotalMiddlePower"
    CondensatorsRelayState(f64),  // "Condensators_reley_output"
    SubStationOneState(f64),   // "462_condensators_state"
    GenCosSetpointOutput(f64),   // "Cos_phi_engine_output"
    GenActivePower(f64),         // "KGU_gen_power"
    GenSetterLocalRemoteState(f64), // "Distance_working_reguler_engine"
    GenEngineRun(f64),       // "KGU_generator_work"
}
