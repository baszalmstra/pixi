# Implementation Plan: Modernize pixi-conda create command

## Overview
Update the `pixi_conda` crate's `create` command to use the latest project standards, specifically replacing the current direct conda solving approach with the `CommandDispatcher` system from `pixi_command_dispatcher`.

## Current State Analysis

### Current Implementation (`crates/pixi_conda/src/create/mod.rs`):
- **Direct Solving**: Uses `rattler_solve` directly with manual repodata fetching
- **Manual Installation**: Uses `rattler::install::Installer` directly  
- **Old Patterns**: Doesn't follow current project conventions
- **No Source Support**: Only handles binary conda packages
- **Duplicated Logic**: Reimplements functionality available in CommandDispatcher

### Target Implementation Pattern:
The `CommandDispatcher` provides:
- `SolveCondaEnvironmentSpec` for solving conda environments
- `InstallPixiEnvironmentSpec` for installing environments  
- Unified caching, progress reporting, and error handling
- Support for both binary and source packages
- Concurrent execution and task deduplication

## Implementation Stages

### Stage 1: ✅ COMPLETE - Setup CommandDispatcher Infrastructure
**Goal**: Set up the basic CommandDispatcher infrastructure for pixi-conda operations
**Success Criteria**: 
- CommandDispatcher builder function created and working
- Progress reporting integrated via pixi_reporters
- Code compiles successfully
**Tests**: Compilation and basic instantiation
**Status**: Complete

**Tasks**:
1. Update `Cargo.toml` to include `pixi_command_dispatcher` dependency
2. Add imports for `CommandDispatcher`, `SolveCondaEnvironmentSpec`
3. Update function signature to accept/create `CommandDispatcher`

### Stage 2: ✅ COMPLETE - Replace Direct Solving with solve_conda_environment
**Goal**: Replace manual repodata fetching and solver task creation with CommandDispatcher's solve_conda_environment
**Success Criteria**:
- Manual gateway queries replaced with CommandDispatcher integration
- SolveCondaEnvironmentSpec properly constructed and used
- Solver task creation eliminated
**Tests**: Solving conda environments works correctly
**Status**: Complete

**Tasks**:
1. Create `SolveCondaEnvironmentSpec` from input parameters
2. Replace manual repodata fetching with CommandDispatcher gateway
3. Replace `rattler_solve` calls with `command_dispatcher.solve_conda_environment()`
4. Update error handling to use `CommandDispatcherError`

### Stage 3: 🔄 IN PROGRESS - Upgrade to solve_pixi_environment
**Goal**: Replace solve_conda_environment with solve_pixi_environment for enhanced flexibility
**Success Criteria**:
- PixiEnvironmentSpec constructed correctly from input requirements
- solve_pixi_environment used instead of solve_conda_environment
- Supports both binary and source packages (future-proofing)
**Tests**: Environment solving works with PixiEnvironmentSpec
**Status**: In Progress

### Stage 4: ⏳ PENDING - Replace Installation with install_pixi_environment
**Goal**: Replace direct Installer usage with CommandDispatcher's install_pixi_environment
**Success Criteria**:
- InstallPixiEnvironmentSpec properly constructed
- Installation uses CommandDispatcher's install_pixi_environment method
- Progress reporting continues to work correctly
**Tests**: Installation completes successfully through CommandDispatcher
**Status**: Pending

**Tasks**:
1. Convert solved records to `InstallPixiEnvironmentSpec`
2. Replace manual `Installer` usage with `command_dispatcher.install_pixi_environment()`
3. Update progress reporting to use CommandDispatcher patterns
4. Remove custom `Reporter` implementation

### Stage 4: Integration and Cleanup
**Goal**: Clean up old code and ensure proper integration
**Success Criteria**: All functionality works with new implementation
**Tests**: All existing tests pass, integration tests work
**Status**: Not Started

**Tasks**:
1. Remove unused imports and dead code
2. Update CLI integration in `cli.rs`
3. Ensure consistent error handling and progress reporting
4. Add proper documentation for the new implementation

### Stage 5: Testing and Validation
**Goal**: Comprehensive testing of the updated implementation
**Success Criteria**: All tests pass and functionality is preserved
**Tests**: Full test suite passes, manual testing confirms functionality
**Status**: Not Started

**Tasks**:
1. Run existing test suite
2. Add tests for new CommandDispatcher integration
3. Perform manual testing with various package specifications
4. Validate that all command-line options work correctly

## Technical Details

### Key Changes Required:

1. **Constructor Pattern**:
   ```rust
   // Old: Direct instantiation
   let gateway = config.gateway(client);
   let solver_result = rattler_solve::resolvo::Solver.solve(task);
   
   // New: CommandDispatcher pattern  
   let command_dispatcher = CommandDispatcher::builder()
       .with_gateway(config.gateway(client))
       .finish();
   let result = command_dispatcher.solve_conda_environment(spec).await;
   ```

2. **Specification Building**:
   ```rust
   // Convert input to SolveCondaEnvironmentSpec
   let spec = SolveCondaEnvironmentSpec {
       name: Some(format!("conda-env-{}", prefix.display())),
       binary_specs: input.match_specs().cloned().collect(),
       platform: platform,
       channels: channels,
       virtual_packages: virtual_packages.into_generic_virtual_packages().collect(),
       // ... other fields
   };
   ```

3. **Installation Pattern**:
   ```rust
   // Convert solved records to installation spec
   let install_spec = InstallPixiEnvironmentSpec {
       prefix: prefix.to_path_buf(),
       records: solved_records,
       // ... other configuration
   };
   let result = command_dispatcher.install_pixi_environment(install_spec).await;
   ```

## Benefits of This Approach

1. **Consistency**: Aligns with current project patterns and standards
2. **Maintainability**: Reduces code duplication and uses shared infrastructure  
3. **Features**: Gains access to caching, progress reporting, and concurrent execution
4. **Future-Proof**: Ready for source package support and other CommandDispatcher features
5. **Testing**: Leverages existing CommandDispatcher test infrastructure

## Notes

- The `input.rs` module can remain largely unchanged as it handles input parsing which is still needed
- Progress reporting and user interaction patterns should be updated to match current project style
- The `EnvironmentName` and `Registry` types can continue to be used for environment management
- Consider whether the `--solver` option should be preserved or if we should use CommandDispatcher's default solver