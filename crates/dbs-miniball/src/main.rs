// Copyright 2022 Alibaba Cloud. All Rights Reserved.
// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

#[cfg(target_arch = "x86_64")]
use std::convert::TryFrom;
#[cfg(target_arch = "x86_64")]
use std::env;

#[cfg(target_arch = "x86_64")]
use api::api::Cli;
#[cfg(target_arch = "x86_64")]
use vmm::vmm::Vmm;

#[cfg(target_arch = "x86_64")]
fn main() {
    match Cli::launch(
        env::args()
            .collect::<Vec<String>>()
            .iter()
            .map(|s| s.as_str())
            .collect(),
    ) {
        Ok(vmm_config) => {
            let mut vmm =
                Vmm::try_from(vmm_config).expect("Failed to create VMM from configurations");
            // For now we are just unwrapping here, in the future we might use a nicer way of
            // handling errors such as pretty printing them.
            vmm.run().unwrap();
        }
        Err(e) => {
            eprintln!("Failed to parse command line options. {}", e);
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn main() {}
