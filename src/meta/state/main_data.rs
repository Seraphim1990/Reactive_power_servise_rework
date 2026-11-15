pub enum MainDataSignal {
    MiddleVoltage(f64),
    MainQ(f64),                     // реактив трансформатора, квар
    MainP(f64),                     // активна потужність трансформатора, кВт
    CondensatorsRelayState(f64),    // по хорошому має бути u16!,
    SubStationOneState(f64),        // має бути bool
    GenCosSetpointOutput(f64),      // теж u16
    GenActivePower(f64),            // u16
    GenSetterLocalRemoteState(f64), // bool
    GenEngineRun(f64),              // bool
}
