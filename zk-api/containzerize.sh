#!/bin/bash

# Script to containerize the zk-api application.
# Supports two modes:
# 1. local: Builds an image for the host architecture, loads it locally,
#           and provides a sample 'docker run' command.
# 2. remote: Builds a multi-arch image and pushes it to Google Artifact Registry.
#
# MUST be run from the workspace root directory.

# --- Workspace Root Check ---
if [[ ! -f "Cargo.toml" ]] || [[ ! -d "zk-api" ]] || [[ ! -d "program" ]] || [[ ! -d "elf" ]]; then
  echo "ERROR: This script MUST be executed from the workspace root directory" >&2
  echo "       (the directory containing Cargo.toml, zk-api/, program/, elf/, etc.)." >&2
  echo "       Example: cd /path/to/workspace && zk-api/containerize.sh" >&2
  echo "Current directory: $(pwd)" >&2
  exit 1
fi

# Exit immediately if a command exits with a non-zero status.
set -e
# Treat unset variables as an error when substituting.
# set -u # Uncomment if you want stricter variable checking
# Ensure pipelines return the status of the last command to exit non-zero.
set -o pipefail

echo "--- zk-api Containerization Script ---"
echo "INFO: Executing from workspace root: $(pwd)"
echo

# --- Mode Selection ---
# Display options clearly
echo "<choose-mode-q>"
echo "[1] build for local testing"
echo "[2] build and deploy to container storage"
read -p "Select mode [1 or 2]: " MODE_INPUT

MODE_LC=""
# Convert input to lowercase using tr for portability
MODE_INPUT_LOWER=$(echo "$MODE_INPUT" | tr '[:upper:]' '[:lower:]')

# Check if input is numeric or text
if [[ "$MODE_INPUT" == "1" ]] || [[ "$MODE_INPUT_LOWER" == "local" ]]; then
    MODE_LC="local"
elif [[ "$MODE_INPUT" == "2" ]] || [[ "$MODE_INPUT_LOWER" == "remote" ]]; then
    MODE_LC="remote"
else
    echo
    echo "ERROR: Invalid mode selected: '$MODE_INPUT'." >&2
    echo "       Please enter '1' or '2'." >&2
    exit 1
fi

# --- Shared Configuration ---
IMAGE_NAME="zk-api"
DOCKERFILE_PATH="zk-api/Dockerfile"

# --- Mode-Specific Logic ---

# LOCAL MODE
if [[ "$MODE_LC" == "local" ]]; then
  echo
  echo "--- Running in LOCAL Mode ---"

  # Determine host architecture for local build
  HOST_ARCH=$(uname -m)
  TARGET_PLATFORM=""
  case "$HOST_ARCH" in
    x86_64)
      TARGET_PLATFORM="linux/amd64"
      ;;
    aarch64 | arm64)
      TARGET_PLATFORM="linux/arm64"
      ;;
    *)
      echo "ERROR: Unsupported host architecture: $HOST_ARCH" >&2
      echo "       Cannot determine target platform for local Docker build." >&2
      exit 1
      ;;
  esac
  echo "Detected host architecture: $HOST_ARCH -> Target Platform: $TARGET_PLATFORM"

  # Define local image tag
  LOCAL_IMAGE_TAG="sp1-helios/${IMAGE_NAME}:local"
  echo "Local image tag will be: ${LOCAL_IMAGE_TAG}"
  echo

  # Build and load locally
  echo "Starting local build for ${TARGET_PLATFORM}..."
  docker buildx build \
    --platform "${TARGET_PLATFORM}" \
    --load \
    -f "${DOCKERFILE_PATH}" \
    -t "${LOCAL_IMAGE_TAG}" \
    .

  echo
  echo "--- Local Build Successful! ---"
  echo "Image built and loaded locally: ${LOCAL_IMAGE_TAG}"
  echo
  echo "To run the container locally:"
  echo "docker run --rm -it -p 8080:8080 --env-file .env ${LOCAL_IMAGE_TAG}"
  echo

# REMOTE MODE
elif [[ "$MODE_LC" == "remote" ]]; then
  echo
  echo "--- Running in REMOTE Mode ---"

  # --- Remote Configuration ---
  GCP_PROJECT_ID="sandbox-zk-api-2223"
  GCP_REGION="us-west1"            # Your Artifact Registry region
  AR_REPO_NAME="zk-api-repo"        # Your Artifact Registry repository name
  PLATFORMS="linux/amd64,linux/arm64" # Platforms for multi-arch build

  echo "Remote Configuration:"
  echo "  Project ID: ${GCP_PROJECT_ID}"
  echo "  Region:     ${GCP_REGION}"
  echo "  Repository: ${AR_REPO_NAME}"
  echo "  Image Name: ${IMAGE_NAME}"
  echo "  Dockerfile: ${DOCKERFILE_PATH}"
  echo "  Platforms:  ${PLATFORMS}"
  echo

  # --- Get Image Tag ---
  # Try to get the short git commit hash as the default tag
  DEFAULT_TAG="latest" # Fallback default
  if git rev-parse --is-inside-work-tree > /dev/null 2>&1; then
    GIT_HASH=$(git rev-parse --short HEAD 2>/dev/null || echo "")
    if [[ -n "$GIT_HASH" ]]; then
        DEFAULT_TAG=$GIT_HASH
        echo "INFO: Using default tag from git commit hash: $DEFAULT_TAG"
    else
        echo "WARNING: Could not get git commit hash, defaulting tag to 'latest'."
    fi
  else
      echo "WARNING: Not inside a git repository, defaulting tag to 'latest'."
  fi

  read -p "Enter the remote image tag [default: ${DEFAULT_TAG}]: " USER_TAG
  IMAGE_TAG=${USER_TAG:-${DEFAULT_TAG}} # Use computed default
  echo "Using tag: ${IMAGE_TAG}"
  echo

  # --- Construct Full Image Name ---
  FULL_IMAGE_TAG="${GCP_REGION}-docker.pkg.dev/${GCP_PROJECT_ID}/${AR_REPO_NAME}/${IMAGE_NAME}:${IMAGE_TAG}"
  echo "Full image name will be: ${FULL_IMAGE_TAG}"
  echo

  # --- Build Step (Remote) ---
  echo "Starting multi-arch image build (cache population)..."
  docker buildx build \
    --platform "${PLATFORMS}" \
    -f "${DOCKERFILE_PATH}" \
    -t "${FULL_IMAGE_TAG}" \
    .

  echo
  echo "--- Build Successful ---"
  echo "Docker image layers built successfully for tag ${IMAGE_TAG} (platforms: ${PLATFORMS})."
  echo

  # --- Confirmation Step (Remote) ---
  read -p "Proceed to pushing to repo ${FULL_IMAGE_TAG}? (Y/N): " CONFIRM
  # Convert confirmation to lowercase using tr for portability
  CONFIRM_LC=$(echo "$CONFIRM" | tr '[:upper:]' '[:lower:]')

  # Check the confirmation input
  if [[ "$CONFIRM_LC" == "y" ]]; then
    # If 'y', proceed silently (the next echo will show progress)
    true
  elif [[ "$CONFIRM_LC" == "n" ]]; then
    echo "Push aborted by user."
    exit 1
  else
    echo "Invalid input. Please enter Y or N." >&2
    exit 1
  fi

  echo
  echo "Proceeding with push..."
  echo

  # --- Push Step (Remote) ---
  docker buildx build \
    --platform "${PLATFORMS}" \
    --push \
    -f "${DOCKERFILE_PATH}" \
    -t "${FULL_IMAGE_TAG}" \
    .

  echo
  echo "--- Push Successful! ---"
  echo "Docker image pushed successfully:"
  echo "${FULL_IMAGE_TAG}"
  echo "Target Platforms: ${PLATFORMS}"

# INVALID MODE
else
  # This else block is now effectively unreachable due to the check after reading input,
  # but kept for structural clarity or future modifications.
  # The primary error handling for invalid input is done right after the read command.
  echo
  echo "ERROR: Invalid mode processing logic reached." >&2 # Should not happen
  exit 1
fi

exit 0
